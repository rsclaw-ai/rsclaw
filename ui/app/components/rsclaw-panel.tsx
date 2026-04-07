"use client";

import { useEffect, useState, useCallback, useRef } from "react";
import { useNavigate, useLocation } from "react-router-dom";
import { ErrorBoundary } from "./error";
import { Popover } from "./ui-lib";
import { toast } from "../lib/toast";
import { EmojiAvatar, AvatarPicker } from "./emoji";
import { IconButton } from "./button";
import ReturnIcon from "../icons/return.svg";
import styles from "./rsclaw-panel.module.scss";
import { Path } from "../constant";
import Locale, { getLang } from "../locales";
import {
  gatewayFetch,
  getHealth,
  getStatus,
  getConfig,
  saveConfig,
  reloadConfig,
  getLogs,
  getAgents,
  saveAgent,
  deleteAgent,
  listWorkspaceFiles,
  readWorkspaceFile,
  writeWorkspaceFile,
  runDoctor,
  runDoctorFix,
  wechatQrStart,
  wechatQrStatus,
  GATEWAY_URL,
  setGatewayUrl,
  setAuthToken,
  testProviderKey,
  listProviderModels,
} from "../lib/rsclaw-api";
import {
  ALL_PROVIDERS,
  ALL_CHANNELS,
  PROV_ORDER_ZH,
  PROV_ORDER_EN,
  CH_ORDER_ZH,
  CH_ORDER_EN,
  MODELS,
} from "./onboarding";
import {
  type ApiType,
  API_TYPE_LABELS,
  API_TYPE_DEFAULT_URLS,
  API_TYPE_NEEDS_KEY,
} from "../lib/provider-defaults";

// ── Types ──────────────────────────────────────────────
interface ChannelInfo {
  type: string;
  name: string;
  status: string;
  detail?: string;
}

interface AgentInfo {
  id: string;
  name?: string;
  avatar?: string;
  model: string;
  status: string;
  toolset?: string[];
  channels?: string[];
}

interface GatewayHealth {
  running: boolean;
  version?: string;
  port?: number;
  uptime?: string;
  memory?: string;
}

interface LogEntry {
  ts: string;
  level: string;
  msg: string;
}

type PanelPage = "status" | "config" | "agents" | "cron" | "skills" | "workspace" | "doctor" | "pairing" | "wizard";

// ── Toggle Component ────────────────────────────────────
function Toggle(props: {
  checked: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <label className={styles["tgl"]}>
      <input
        type="checkbox"
        className={styles["tgl-input"]}
        checked={props.checked}
        onChange={(e) => props.onChange(e.target.checked)}
      />
      <span className={styles["tgl-track"]} />
      <span className={styles["tgl-knob"]} />
    </label>
  );
}

// ══════════════════════════════════════════════════════════
// ── Status Page ──────────────────────────────────────────
// ══════════════════════════════════════════════════════════
function StatusPage() {
  const [health, setHealth] = useState<GatewayHealth>({ running: false });
  const [channels, setChannels] = useState<ChannelInfo[]>([]);
  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [logs, setLogs] = useState<LogEntry[]>([]);
  const [logPaused, setLogPaused] = useState(false);
  const [sessions, setSessions] = useState(0);
  const logBoxRef = useRef<HTMLDivElement>(null);

  const fetchData = useCallback(async () => {
    try {
      const healthData = await getHealth();
      setHealth({
        running: true,
        version: healthData.version || "unknown",
        port: healthData.port || 18888,
        uptime: healthData.uptime || "",
        memory: healthData.memory || "",
      });
    } catch {
      setHealth({ running: false });
    }

    try {
      const statusData = await getStatus();
      setAgents(statusData.agents || []);
      setChannels(statusData.channels || []);
      setSessions(statusData.sessions || 0);
      if (statusData.memory) {
        setHealth((h) => ({ ...h, memory: statusData.memory }));
      }
    } catch {
      // status endpoint may not be available
    }
  }, []);

  const fetchLogs = useCallback(async () => {
    if (logPaused) return;
    try {
      const logData = await getLogs(30);
      if (Array.isArray(logData)) {
        setLogs(logData);
      } else if (logData.logs) {
        setLogs(logData.logs);
      }
    } catch {
      // logs endpoint may not be available
    }
  }, [logPaused]);

  useEffect(() => {
    fetchData();
    const interval = setInterval(fetchData, 8000);
    return () => clearInterval(interval);
  }, [fetchData]);

  useEffect(() => {
    fetchLogs();
    const interval = setInterval(fetchLogs, 3000);
    return () => clearInterval(interval);
  }, [fetchLogs]);

  useEffect(() => {
    if (logBoxRef.current && !logPaused) {
      logBoxRef.current.scrollTop = logBoxRef.current.scrollHeight;
    }
  }, [logs, logPaused]);

  const [modalAction, setModalAction] = useState<"stop" | "restart" | "start" | null>(null);
  const [actionLoading, setActionLoading] = useState(false);

  const executeAction = async (action: string) => {
    setActionLoading(true);
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) {
        if (action === "stop") {
          const { setUserStopped } = await import("./sidebar");
          setUserStopped(true);
          await tauriInvoke("stop_gateway");
        } else if (action === "restart") {
          await tauriInvoke("stop_gateway");
          await new Promise((r) => setTimeout(r, 1500));
          await tauriInvoke("start_gateway");
        } else if (action === "start") {
          const { setUserStopped } = await import("./sidebar");
          setUserStopped(false);
          await tauriInvoke("start_gateway");
        }
      } else {
        // Web mode: no sidecar, just show status
      }
      await new Promise((r) => setTimeout(r, 2000));
      fetchData();
      const msg = action === "stop" ? Locale.RsClawPanel.Status.StopTitle
        : action === "restart" ? Locale.RsClawPanel.Status.RestartTitle
        : Locale.RsClawPanel.Status.StartTitle;
      toast.success(msg);
    } catch (e) {
      toast.fromError("", e);
    } finally {
      setActionLoading(false);
      setModalAction(null);
    }
  };

  const channelIcon = (type: string) => {
    const zh = getLang() === "cn";
    const map: Record<string, { label: string; name: string }> = {
      wechat: { label: "W", name: zh ? "微信" : "WeChat" },
      feishu: { label: "F", name: zh ? "飞书" : "Feishu" },
      telegram: { label: "Tg", name: "Telegram" },
      dingtalk: { label: "DT", name: zh ? "钉钉" : "DingTalk" },
      discord: { label: "Dc", name: "Discord" },
      slack: { label: "Sl", name: "Slack" },
      wecom: { label: "WC", name: zh ? "企业微信" : "WeCom" },
      matrix: { label: "Mx", name: "Matrix" },
    };
    return map[type] || { label: type.slice(0, 2).toUpperCase(), name: type };
  };

  return (
    <div>
      {/* Status Banner */}
      <div className={styles["status-banner"]}>
        <div
          className={`${styles["status-led"]} ${health.running ? styles["on"] : styles["off"]}`}
        />
        <div className={styles["status-info"]}>
          <div className={styles["status-name"]}>{Locale.RsClawPanel.Status.GatewayName}</div>
          <div className={styles["status-addr"]}>
            {health.running
              ? `rsclaw gateway v${health.version || "?"} · localhost:${health.port || 18888} · ${Locale.RsClawPanel.Status.Uptime} ${health.uptime || "N/A"}`
              : Locale.RsClawPanel.Status.NotResponding}
          </div>
        </div>
        <>
          {!health.running && (
            <button className={styles["btn"]} onClick={() => setModalAction("start")}>
              {Locale.RsClawPanel.Status.Start}
            </button>
          )}
          <button className={styles["btn"]} onClick={() => setModalAction("restart")}>
            {Locale.RsClawPanel.Status.Restart}
          </button>
          <button className={`${styles["btn"]} ${styles["danger"]}`} onClick={() => setModalAction("stop")}>
            {Locale.RsClawPanel.Status.Stop}
          </button>
        </>
      </div>

      {/* Stats Row */}
      <div className={styles["stat-row"]}>
        <div className={styles["stat-c"]}>
          <div className={styles["stat-lbl"]}>{Locale.RsClawPanel.Status.ConnectedChannels}</div>
          <div className={`${styles["stat-val"]} ${styles["g"]}`}>
            {channels.filter((c) => c.status === "connected").length}
          </div>
        </div>
        <div className={styles["stat-c"]}>
          <div className={styles["stat-lbl"]}>{Locale.RsClawPanel.Status.ActiveSessions}</div>
          <div className={styles["stat-val"]}>{sessions}</div>
          <div className={styles["stat-sub"]}>{Locale.RsClawPanel.Status.Last24h}</div>
        </div>
        <div className={styles["stat-c"]}>
          <div className={styles["stat-lbl"]}>{Locale.RsClawPanel.Status.Memory}</div>
          <div className={`${styles["stat-val"]} ${styles["a"]}`}>
            {health.memory || "--"}
          </div>
          <div className={styles["stat-sub"]}>{Locale.RsClawPanel.Status.SingleProcess}</div>
        </div>
      </div>

      {/* Channel List */}
      <div className={styles["card"]}>
        <div className={styles["card-h"]}>
          <div className={styles["card-ht"]}>{Locale.RsClawPanel.Status.MessageChannels}</div>
        </div>
        {channels.length > 0 ? (
          channels.map((ch, i) => {
            const icon = channelIcon(ch.type);
            return (
              <div key={i} className={styles["ch-row"]}>
                <div
                  className={styles["ch-ico"]}
                  style={{ background: "rgba(249,115,22,0.15)", color: "#f97316", border: "1px solid rgba(249,115,22,0.25)" }}
                >
                  {icon.label}
                </div>
                <div>
                  <div className={styles["ch-n"]}>{icon.name}</div>
                  <div className={styles["ch-s"]}>{ch.detail || ch.type}</div>
                </div>
                <span
                  className={`${styles["pill"]} ${
                    ch.status === "connected"
                      ? styles["on"]
                      : ch.status === "error"
                        ? styles["warn"]
                        : styles["off"]
                  }`}
                >
                  {ch.status}
                </span>
              </div>
            );
          })
        ) : (
          <div className={styles["ch-row"]}>
            <div className={styles["ch-s"]}>
              {health.running
                ? Locale.RsClawPanel.Status.NoChannels
                : Locale.RsClawPanel.Status.StartGateway}
            </div>
          </div>
        )}
      </div>

      {/* Log Viewer */}
      <div className={styles["card"]}>
        <div className={styles["card-h"]}>
          <div className={styles["card-ht"]}>{Locale.RsClawPanel.Status.RealtimeLogs}</div>
          <div className={styles["card-hr"]}>
            <button
              className={styles["btn"]}
              onClick={() => setLogPaused(!logPaused)}
            >
              {logPaused ? Locale.RsClawPanel.Status.Resume : Locale.RsClawPanel.Status.Pause}
            </button>
            <button className={styles["btn"]} onClick={() => setLogs([])}>
              {Locale.RsClawPanel.Status.Clear}
            </button>
          </div>
        </div>
        <div className={styles["log-box"]} ref={logBoxRef}>
          {logs.length > 0 ? (
            logs.map((log, i) => (
              <div key={i} className={styles["ll"]}>
                <span className={styles["lts"]}>{log.ts}</span>
                <span
                  className={`${styles["llv"]} ${
                    styles[
                      log.level === "OK"
                        ? "ok"
                        : log.level === "INFO"
                          ? "info"
                          : log.level === "WARN"
                            ? "warn"
                            : log.level === "ERROR"
                              ? "err"
                              : "info"
                    ]
                  }`}
                >
                  {log.level}
                </span>
                <span className={styles["lm"]}>{log.msg}</span>
              </div>
            ))
          ) : (
            <div className={styles["lm"]}>
              {health.running
                ? Locale.RsClawPanel.Status.WaitingLogs
                : Locale.RsClawPanel.Status.GatewayNotRunning}
            </div>
          )}
        </div>
      </div>

      {/* Gateway Action Modal */}
      {modalAction && (
        <div className={styles["gw-modal-overlay"]} onClick={() => !actionLoading && setModalAction(null)}>
          <div className={styles["gw-modal"]} onClick={(e) => e.stopPropagation()}>
            <div className={styles["gw-modal-body"]}>
              <div className={`${styles["gw-modal-icon"]} ${
                modalAction === "stop" ? styles["red"] : modalAction === "restart" ? styles["amber"] : styles["green"]
              }`}>
                {modalAction === "stop" ? "\u23F9" : modalAction === "restart" ? "\uD83D\uDD04" : "\u25B6"}
              </div>
              <div className={styles["gw-modal-title"]}>
                {modalAction === "stop" ? Locale.RsClawPanel.Status.StopTitle
                  : modalAction === "restart" ? Locale.RsClawPanel.Status.RestartTitle
                  : Locale.RsClawPanel.Status.StartTitle}
              </div>
              <div className={styles["gw-modal-sub"]}>
                {modalAction === "stop" ? Locale.RsClawPanel.Status.StopSub
                  : modalAction === "restart" ? Locale.RsClawPanel.Status.RestartSub
                  : Locale.RsClawPanel.Status.StartSub}
              </div>
              {health.running && (
                <div className={styles["gw-modal-status"]}>
                  <div className={styles["gw-modal-stat"]}>
                    <div className={styles["gw-modal-stat-label"]}>{Locale.RsClawPanel.Status.CurrentStatus}</div>
                    <div className={styles["gw-modal-stat-val"]} style={{ color: "#2dd4a0" }}>{Locale.RsClawPanel.Running}</div>
                  </div>
                  <div className={styles["gw-modal-stat"]}>
                    <div className={styles["gw-modal-stat-label"]}>{"Version"}</div>
                    <div className={styles["gw-modal-stat-val"]} style={{ color: "#6b6877" }}>v{health.version || "unknown"}</div>
                  </div>
                  <div className={styles["gw-modal-stat"]}>
                    <div className={styles["gw-modal-stat-label"]}>{Locale.RsClawPanel.Status.ChannelCount}</div>
                    <div className={styles["gw-modal-stat-val"]} style={{ color: "#f0a500" }}>
                      {channels.filter(c => c.status === "connected").length} {Locale.RsClawPanel.Status.Unit}
                    </div>
                  </div>
                  <div className={styles["gw-modal-stat"]}>
                    <div className={styles["gw-modal-stat-label"]}>{Locale.RsClawPanel.Status.SessionCount}</div>
                    <div className={styles["gw-modal-stat-val"]} style={{ color: "#f0a500" }}>{sessions} {Locale.RsClawPanel.Status.Unit}</div>
                  </div>
                  <div className={styles["gw-modal-stat"]}>
                    <div className={styles["gw-modal-stat-label"]}>{Locale.RsClawPanel.Status.UptimeLabel}</div>
                    <div className={styles["gw-modal-stat-val"]} style={{ color: "#6b6877" }}>{health.uptime || "--"}</div>
                  </div>
                </div>
              )}
              <div className={`${styles["gw-modal-note"]} ${
                modalAction === "stop" ? styles["danger"] : modalAction === "restart" ? styles["warn"] : styles["ok"]
              }`}>
                <span>{modalAction === "stop" ? "\u26A0" : modalAction === "restart" ? "\uD83D\uDD04" : "\u2139"}</span>
                <span>
                  {modalAction === "stop" ? Locale.RsClawPanel.Status.StopNote
                    : modalAction === "restart" ? Locale.RsClawPanel.Status.RestartNote
                    : Locale.RsClawPanel.Status.StartNote}
                </span>
              </div>
            </div>
            <div className={styles["gw-modal-actions"]}>
              <button className={styles["gw-btn-cancel"]} onClick={() => setModalAction(null)} disabled={actionLoading}>
                {Locale.RsClawPanel.Status.Cancel}
              </button>
              <button
                className={`${styles["gw-btn-confirm"]} ${styles[modalAction]}`}
                onClick={() => executeAction(modalAction)}
                disabled={actionLoading}
              >
                {actionLoading && <span className={styles["gw-spinner"]} />}
                {actionLoading
                  ? (modalAction === "stop" ? Locale.RsClawPanel.Status.Stopping
                    : modalAction === "restart" ? Locale.RsClawPanel.Status.Restarting
                    : Locale.RsClawPanel.Status.Starting)
                  : (modalAction === "stop" ? Locale.RsClawPanel.Status.StopConfirm
                    : modalAction === "restart" ? Locale.RsClawPanel.Status.RestartConfirm
                    : Locale.RsClawPanel.Status.StartConfirm)}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ══════════════════════════════════════════════════════════
// ── Config Editor Page ───────────────────────────────────
// ══════════════════════════════════════════════════════════
function ConfigEditorPage() {
  type TabKey = "gateway" | "models" | "channels" | "tools";
  const tabs: { key: TabKey; label: string }[] = [
    { key: "gateway", label: getLang() === "cn" ? "\u7F51\u5173" : "Gateway" },
    { key: "models", label: getLang() === "cn" ? "\u6A21\u578B\u63D0\u4F9B\u5546" : "Models" },
    { key: "channels", label: getLang() === "cn" ? "\u6D88\u606F\u901A\u9053" : "Channels" },
    { key: "tools", label: getLang() === "cn" ? "\u5DE5\u5177 & \u529F\u80FD" : "Tools" },
  ];

  const [activeTab, setActiveTab] = useState<TabKey>("gateway");
  const [rawMode, setRawMode] = useState(() => typeof window !== "undefined" && !!(window as any).__TAURI__);
  const [rawConfig, setRawConfig] = useState("");
  const [configPath, setConfigPath] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [dirty, setDirty] = useState(false);

  // Parsed config fields - Gateway
  const [port, setPort] = useState(18889);
  const [bind, setBind] = useState("127.0.0.1");
  const [language, setLanguage] = useState("zh-CN");
  const [processingTimeout, setProcessingTimeout] = useState(120);
  const [authToken, setAuthToken] = useState("");
  const [agentModel, setAgentModel] = useState("");
  const [agentMaxTokens, setAgentMaxTokens] = useState(4096);

  // Parsed config fields - Models
  const [providers, setProviders] = useState<{
    name: string; key: string; enabled: boolean; apiKey: string; baseUrl: string; apiType?: ApiType;
  }[]>([
    { name: "Anthropic", key: "anthropic", enabled: false, apiKey: "", baseUrl: "" },
    { name: "OpenAI", key: "openai", enabled: false, apiKey: "", baseUrl: "" },
    { name: "DeepSeek", key: "deepseek", enabled: false, apiKey: "", baseUrl: "" },
    { name: "Doubao (\u8C46\u5305)", key: "doubao", enabled: false, apiKey: "", baseUrl: "" },
    { name: "Ollama", key: "ollama", enabled: true, apiKey: "", baseUrl: "http://localhost:11434" },
    { name: "Custom Provider", key: "custom", enabled: false, apiKey: "", baseUrl: "" },
  ]);

  // Parsed config fields - Channels
  const [channels, setChannels] = useState<{
    type: string; enabled: boolean; status: string;
  }[]>([]);

  // Parsed config fields - Tools
  const [execSandbox, setExecSandbox] = useState(true);
  const [uploadMaxSize, setUploadMaxSize] = useState(10);
  const [webSearchProvider, setWebSearchProvider] = useState("none");
  const [memoryShortTermLimit, setMemoryShortTermLimit] = useState(20);
  const [memoryLongTermLimit, setMemoryLongTermLimit] = useState(100);

  // Simple JSON5 parser helper - extract value for a key from raw text
  const extractVal = useCallback((raw: string, key: string): string | undefined => {
    // Match key: value or "key": value patterns in json5
    const patterns = [
      new RegExp(`["']?${key}["']?\\s*:\\s*"([^"]*)"`, "m"),
      new RegExp(`["']?${key}["']?\\s*:\\s*'([^']*)'`, "m"),
      new RegExp(`["']?${key}["']?\\s*:\\s*([\\d.]+)`, "m"),
      new RegExp(`["']?${key}["']?\\s*:\\s*(true|false)`, "m"),
    ];
    for (const p of patterns) {
      const m = raw.match(p);
      if (m) return m[1];
    }
    return undefined;
  }, []);

  const parseConfig = useCallback((raw: string) => {
    // Gateway fields
    const p = extractVal(raw, "port");
    if (p) setPort(parseInt(p, 10) || 18889);
    const b = extractVal(raw, "bind");
    if (b) setBind(b);
    const l = extractVal(raw, "language");
    if (l) setLanguage(l);
    const pt = extractVal(raw, "processingTimeout");
    if (pt) setProcessingTimeout(parseInt(pt, 10) || 120);
    const at = extractVal(raw, "authToken");
    if (at) setAuthToken(at);

    // Agent defaults
    const am = extractVal(raw, "model");
    if (am) setAgentModel(am);
    const amt = extractVal(raw, "maxTokens");
    if (amt) setAgentMaxTokens(parseInt(amt, 10) || 4096);

    // Models - try to detect provider blocks
    const providerNames = ["anthropic", "openai", "deepseek", "doubao", "ollama", "custom"];
    const displayNames: Record<string, string> = {
      anthropic: "Anthropic", openai: "OpenAI", deepseek: "DeepSeek", doubao: "Doubao (\u8C46\u5305)", ollama: "Ollama", custom: "Custom Provider",
    };
    const newProviders = providerNames.map((pName) => {
      // Look for a block like anthropic: { ... }
      const blockRe = new RegExp(`["']?${pName}["']?\\s*:\\s*\\{([^}]*)\\}`, "ms");
      const blockMatch = raw.match(blockRe);
      const block = blockMatch ? blockMatch[1] : "";
      const apiKey = extractVal(block, "apiKey") || "";
      const baseUrl = extractVal(block, "baseUrl") || extractVal(block, "base_url") || "";
      const enabled = extractVal(block, "enabled");
      const apiField = extractVal(block, "api") || extractVal(block, "api_type");
      const apiType: ApiType | undefined = (apiField === "anthropic" || apiField === "gemini" || apiField === "ollama" || apiField === "openai" || apiField === "openai-responses")
        ? (apiField as ApiType)
        : undefined;
      return {
        name: displayNames[pName] || pName,
        key: pName,
        enabled: enabled !== undefined ? enabled === "true" : (pName === "custom" ? !!baseUrl : apiKey.length > 0),
        apiKey,
        baseUrl: baseUrl || "",
        ...(pName === "custom" ? { apiType } : {}),
      };
    });
    setProviders(newProviders);

    // Channels - detect channel blocks
    const channelTypes = ["wechat", "wecom", "telegram", "slack", "dingtalk", "feishu", "http", "terminal"];
    const detectedChannels: { type: string; enabled: boolean; status: string }[] = [];
    for (const ct of channelTypes) {
      const re = new RegExp(`["']?${ct}["']?\\s*:\\s*\\{`, "m");
      if (raw.match(re)) {
        const blockRe = new RegExp(`["']?${ct}["']?\\s*:\\s*\\{([^}]*)\\}`, "ms");
        const bm = raw.match(blockRe);
        const block = bm ? bm[1] : "";
        const en = extractVal(block, "enabled");
        detectedChannels.push({
          type: ct,
          enabled: en !== undefined ? en === "true" : true,
          status: en === "false" ? "disabled" : "configured",
        });
      }
    }
    setChannels(detectedChannels);

    // Tools
    const es = extractVal(raw, "sandbox");
    if (es !== undefined) setExecSandbox(es === "true");
    const ums = extractVal(raw, "maxUploadSize");
    if (ums) setUploadMaxSize(parseInt(ums, 10) || 10);
    const wsp = extractVal(raw, "webSearchProvider") || extractVal(raw, "searchProvider");
    if (wsp) setWebSearchProvider(wsp);
    const stl = extractVal(raw, "shortTermLimit");
    if (stl) setMemoryShortTermLimit(parseInt(stl, 10) || 20);
    const ltl = extractVal(raw, "longTermLimit");
    if (ltl) setMemoryLongTermLimit(parseInt(ltl, 10) || 100);
  }, [extractVal]);

  useEffect(() => {
    (async () => {
      try {
        const data = await getConfig();
        if (data.raw) {
          setRawConfig(data.raw);
          setConfigPath(data.path || "");
          parseConfig(data.raw);
        } else {
          throw new Error("no raw config");
        }
      } catch {
        // Fallback: read config file directly via Tauri
        try {
          const tauriInvoke = (window as any).__TAURI__?.invoke;
          if (tauriInvoke) {
            const cp: string = await tauriInvoke("get_config_path");
            const home = cp || "~/.rsclaw";
            const configFile = home + "/rsclaw.json5";
            // Use rsclaw config get to read raw content
            const raw: string = await tauriInvoke("read_config_file");
            if (raw) {
              setRawConfig(raw);
              setConfigPath(configFile);
              parseConfig(raw);
            } else {
              setRawConfig("// Failed to load config");
            }
          } else {
            setRawConfig("// Failed to load config (gateway auth required)");
          }
        } catch {
          setRawConfig("// Failed to load config");
        }
      } finally {
        setLoading(false);
      }
    })();
  }, [parseConfig]);

  // Rebuild raw config from structured fields is complex for json5.
  // Instead, we do targeted find-replace on the raw text for known keys.
  const applyFieldToRaw = useCallback((raw: string, key: string, value: string | number | boolean): string => {
    const strVal = typeof value === "string" ? `"${value}"` : String(value);
    // Try to find and replace existing key
    const patterns = [
      new RegExp(`(["']?${key}["']?\\s*:\\s*)"[^"]*"`, "m"),
      new RegExp(`(["']?${key}["']?\\s*:\\s*)'[^']*'`, "m"),
      new RegExp(`(["']?${key}["']?\\s*:\\s*)[\\d.]+`, "m"),
      new RegExp(`(["']?${key}["']?\\s*:\\s*)(?:true|false)`, "m"),
    ];
    for (const p of patterns) {
      if (raw.match(p)) {
        return raw.replace(p, `$1${strVal}`);
      }
    }
    return raw; // key not found, leave unchanged
  }, []);

  const buildRawFromFields = useCallback(() => {
    let raw = rawConfig;
    raw = applyFieldToRaw(raw, "port", port);
    raw = applyFieldToRaw(raw, "bind", bind);
    raw = applyFieldToRaw(raw, "language", language);
    raw = applyFieldToRaw(raw, "processingTimeout", processingTimeout);
    if (authToken) raw = applyFieldToRaw(raw, "authToken", authToken);
    return raw;
  }, [rawConfig, port, bind, language, processingTimeout, authToken, applyFieldToRaw]);

  const handleSave = async () => {
    setSaving(true);
    try {
      const finalRaw = rawMode ? rawConfig : buildRawFromFields();
      const result = await saveConfig({ raw: finalRaw });
      if (result.error) {
        toast.error(Locale.RsClawPanel.Config.SaveFailed, result.error);
      } else {
        setDirty(false);
        setRawConfig(finalRaw);
        toast.success(Locale.RsClawPanel.Config.SaveSuccess);
        try { await reloadConfig(); } catch {}
      }
    } catch (e) {
      toast.fromError(Locale.RsClawPanel.Config.SaveFailed, e);
    } finally {
      setSaving(false);
    }
  };

  const markDirty = () => { if (!dirty) setDirty(true); };

  if (loading) return <div className={styles["empty-state"]}>{Locale.RsClawPanel.Config.Loading}</div>;

  // Safety: if raw config couldn't be loaded or starts with error comment
  if (!rawConfig || rawConfig.startsWith("//")) {
    return (
      <div style={{ padding: 20 }}>
        <div style={{ fontSize: 14, color: "#d95f5f", marginBottom: 12 }}>{rawConfig || "No config loaded"}</div>
        <div style={{ fontSize: 12, color: "#888" }}>
          {getLang() === "cn" ? "\u65E0\u6CD5\u52A0\u8F7D\u914D\u7F6E\uFF0C\u8BF7\u786E\u8BA4\u7F51\u5173\u5DF2\u542F\u52A8\u5E76\u4E14 auth token \u6B63\u786E\u3002" : "Cannot load config. Ensure gateway is running and auth token is correct."}
        </div>
      </div>
    );
  }


  // ---- Inline style constants ----
  const sectionCard: React.CSSProperties = {
    background: "var(--white)", borderRadius: "12px", border: "1px solid var(--border-in-light)",
    padding: "20px", marginBottom: "16px",
  };
  const sectionTitle: React.CSSProperties = {
    fontSize: "14px", fontWeight: 600, color: "var(--black)", marginBottom: "16px",
    paddingBottom: "10px", borderBottom: "1px solid var(--border-in-light)",
  };
  const fieldRow: React.CSSProperties = {
    display: "flex", alignItems: "center", justifyContent: "space-between",
    padding: "10px 0", borderBottom: "1px solid rgba(0,0,0,0.04)",
  };
  const fieldLabel: React.CSSProperties = {
    fontSize: "13px", color: "var(--black)", fontWeight: 500, minWidth: "140px",
  };
  const fieldSub: React.CSSProperties = {
    fontSize: "11px", color: "#999", marginTop: "2px",
  };
  const fieldInput: React.CSSProperties = {
    padding: "6px 10px", borderRadius: "6px", border: "1px solid var(--border-in-light)",
    fontSize: "13px", background: "var(--white)", outline: "none", width: "220px",
  };
  const fieldSelect: React.CSSProperties = { ...fieldInput, width: "230px" };
  const toggleTrack = (on: boolean): React.CSSProperties => ({
    width: "40px", height: "22px", borderRadius: "11px", cursor: "pointer",
    background: on ? "#f0a500" : "#ccc", position: "relative", transition: "background 0.2s",
    display: "inline-block", flexShrink: 0,
  });
  const toggleThumb = (on: boolean): React.CSSProperties => ({
    width: "18px", height: "18px", borderRadius: "50%", background: "#fff",
    position: "absolute", top: "2px", left: on ? "20px" : "2px", transition: "left 0.2s",
    boxShadow: "0 1px 3px rgba(0,0,0,0.2)",
  });
  const providerCard: React.CSSProperties = {
    border: "1px solid var(--border-in-light)", borderRadius: "10px", padding: "16px",
    marginBottom: "12px", background: "var(--white)",
  };
  const providerHeader: React.CSSProperties = {
    display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: "12px",
  };
  const providerName: React.CSSProperties = { fontSize: "14px", fontWeight: 600 };
  const channelRow: React.CSSProperties = {
    display: "flex", alignItems: "center", justifyContent: "space-between",
    padding: "12px 16px", borderRadius: "8px", border: "1px solid var(--border-in-light)",
    marginBottom: "8px", background: "var(--white)",
  };
  const statusPill = (status: string): React.CSSProperties => ({
    padding: "2px 10px", borderRadius: "10px", fontSize: "11px", fontWeight: 500,
    background: status === "configured" ? "rgba(45,212,160,0.12)" : status === "pending" ? "rgba(240,165,0,0.12)" : "rgba(0,0,0,0.06)",
    color: status === "configured" ? "#2dd4a0" : status === "pending" ? "#f0a500" : "#999",
  });
  const sliderContainer: React.CSSProperties = {
    display: "flex", alignItems: "center", gap: "10px", width: "220px",
  };

  // Toggle component
  const Toggle = ({ on, onToggle }: { on: boolean; onToggle: () => void }) => (
    <div style={toggleTrack(on)} onClick={onToggle}>
      <div style={toggleThumb(on)} />
    </div>
  );

  // ---- Tab content renderers ----
  const renderGateway = () => (
    <div>
      <div style={sectionCard}>
        <div style={sectionTitle}>{Locale.RsClawPanel.Config.Gateway}</div>
        <div style={fieldRow}>
          <div>
            <div style={fieldLabel}>{Locale.RsClawPanel.Config.Port}</div>
            <div style={fieldSub}>HTTP API port</div>
          </div>
          <input style={fieldInput} type="number" value={port}
            onChange={(e) => { setPort(parseInt(e.target.value, 10) || 18889); markDirty(); }} />
        </div>
        <div style={fieldRow}>
          <div>
            <div style={fieldLabel}>{Locale.RsClawPanel.Config.Bind}</div>
            <div style={fieldSub}>{Locale.RsClawPanel.Config.BindLoopback} / {Locale.RsClawPanel.Config.BindAll}</div>
          </div>
          <select style={fieldSelect} value={bind}
            onChange={(e) => { setBind(e.target.value); markDirty(); }}>
            <option value="127.0.0.1">{Locale.RsClawPanel.Config.BindLoopback}</option>
            <option value="0.0.0.0">{Locale.RsClawPanel.Config.BindAll}</option>
          </select>
        </div>
        <div style={fieldRow}>
          <div>
            <div style={fieldLabel}>{Locale.RsClawPanel.Config.Language}</div>
          </div>
          <select style={fieldSelect} value={language}
            onChange={(e) => { setLanguage(e.target.value); markDirty(); }}>
            <option value="zh-CN">zh-CN</option>
            <option value="en">en</option>
            <option value="ja">ja</option>
          </select>
        </div>
        <div style={fieldRow}>
          <div>
            <div style={fieldLabel}>{Locale.RsClawPanel.Config.ProcessingTimeout}</div>
            <div style={fieldSub}>seconds</div>
          </div>
          <input style={fieldInput} type="number" value={processingTimeout}
            onChange={(e) => { setProcessingTimeout(parseInt(e.target.value, 10) || 120); markDirty(); }} />
        </div>
        <div style={{ ...fieldRow, borderBottom: "none" }}>
          <div>
            <div style={fieldLabel}>Auth Token</div>
            <div style={fieldSub}>API authentication token</div>
          </div>
          <input style={fieldInput} type="password" value={authToken} placeholder="(not set)"
            onChange={(e) => { setAuthToken(e.target.value); markDirty(); }} />
        </div>
      </div>
      <div style={sectionCard}>
        <div style={sectionTitle}>Agent Defaults</div>
        <div style={fieldRow}>
          <div>
            <div style={fieldLabel}>Default Model</div>
          </div>
          <input style={fieldInput} value={agentModel} placeholder="e.g. claude-sonnet-4-20250514"
            onChange={(e) => { setAgentModel(e.target.value); markDirty(); }} />
        </div>
        <div style={{ ...fieldRow, borderBottom: "none" }}>
          <div>
            <div style={fieldLabel}>Max Tokens</div>
          </div>
          <input style={fieldInput} type="number" value={agentMaxTokens}
            onChange={(e) => { setAgentMaxTokens(parseInt(e.target.value, 10) || 4096); markDirty(); }} />
        </div>
      </div>
    </div>
  );

  const renderModels = () => (
    <div>
      {providers.map((prov, idx) => {
        const isCustom = prov.key === "custom";
        const curApiType: ApiType | undefined = prov.apiType;
        const hideKey = prov.key === "ollama";
        const keyOptional = isCustom && curApiType && !API_TYPE_NEEDS_KEY[curApiType];
        const isZh = getLang() === "cn";
        // Determine configuration status
        const hasCredentials = prov.apiKey.length > 0 || (["ollama", "custom"].includes(prov.key) && prov.baseUrl.length > 0);
        const badgeLabel = prov.enabled
          ? (hasCredentials ? (isZh ? "已配置" : "Configured") : (isZh ? "待配置" : "Pending"))
          : (isZh ? "关闭" : "OFF");
        const badgeBg = prov.enabled
          ? (hasCredentials ? "rgba(45,212,160,0.12)" : "rgba(240,165,0,0.12)")
          : "rgba(0,0,0,0.06)";
        const badgeColor = prov.enabled
          ? (hasCredentials ? "#2dd4a0" : "#f0a500")
          : "#999";
        return (
        <div key={prov.key} style={providerCard}>
          <div style={providerHeader}>
            <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
              <span style={providerName}>{prov.name}</span>
              <span style={{
                padding: "2px 8px", borderRadius: "8px", fontSize: "11px",
                background: badgeBg,
                color: badgeColor,
              }}>
                {badgeLabel}
              </span>
            </div>
            <Toggle on={prov.enabled} onToggle={() => {
              const next = [...providers];
              next[idx] = { ...next[idx], enabled: !next[idx].enabled };
              setProviders(next);
              markDirty();
            }} />
          </div>
          {isCustom && (
            <div style={fieldRow}>
              <div style={fieldLabel}>API Type</div>
              <select
                style={{ ...fieldInput, cursor: "pointer" }}
                value={curApiType || ""}
                onChange={(e) => {
                  const val = e.target.value;
                  if (!val) return;
                  const at = val as ApiType;
                  const next = [...providers];
                  next[idx] = { ...next[idx], apiType: at, baseUrl: "" };
                  setProviders(next);
                  markDirty();
                }}
              >
                {!curApiType && <option value="">-- Select --</option>}
                {(Object.keys(API_TYPE_LABELS) as ApiType[]).map((at) => (
                  <option key={at} value={at}>{API_TYPE_LABELS[at]}</option>
                ))}
              </select>
            </div>
          )}
          {!hideKey && (
          <div style={fieldRow}>
              <div style={fieldLabel}>API Key{keyOptional ? <span style={{ color: "#999", fontWeight: 400 }}> (optional)</span> : null}</div>
              <input style={fieldInput} type="password" value={prov.apiKey}
                placeholder={keyOptional ? "(optional)" : "sk-..."}
                onChange={(e) => {
                  const next = [...providers];
                  next[idx] = { ...next[idx], apiKey: e.target.value };
                  setProviders(next);
                  markDirty();
                }} />
            </div>
          )}
          {(isCustom || prov.key === "doubao" || prov.key === "ollama") && (
          <div style={{ ...fieldRow, borderBottom: "none" }}>
            <div style={fieldLabel}>API URL</div>
            <input style={fieldInput} value={prov.baseUrl}
              placeholder={
                isCustom ? "https://your-api-server.com" :
                prov.key === "doubao" ? "https://ark.cn-beijing.volces.com/api/v3" :
                prov.key === "ollama" ? "http://localhost:11434" :
                "(default)"
              }
              onChange={(e) => {
                const next = [...providers];
                next[idx] = { ...next[idx], baseUrl: e.target.value };
                setProviders(next);
                markDirty();
              }} />
          </div>
          )}
        </div>
        );
      })}
    </div>
  );

  // All 13 channels with their credential fields
  const zh = getLang() === "cn";
  const ALL_CHANNELS_DEF = [
    { id: "wechat", icon: "\u5FAE", name: zh ? "\u5FAE\u4FE1" : "WeChat", fields: [
      { key: "botId", label: "Bot ID", type: "text", placeholder: "xxx@im.bot" },
      { key: "botToken", label: "Bot Token", type: "password", placeholder: "${WECHAT_BOT_TOKEN}" },
    ]},
    { id: "wecom", icon: "WC", name: zh ? "\u4F01\u4E1A\u5FAE\u4FE1" : "WeCom", fields: [
      { key: "botId", label: "Bot ID", type: "text", placeholder: "" },
      { key: "secret", label: "Secret", type: "password", placeholder: "${WECOM_SECRET}" },
    ]},
    { id: "feishu", icon: "\u98DE", name: zh ? "\u98DE\u4E66" : "Feishu", fields: [
      { key: "appId", label: "App ID", type: "text", placeholder: "cli_xxx" },
      { key: "appSecret", label: "App Secret", type: "password", placeholder: "${FEISHU_APP_SECRET}" },
      { key: "brand", label: "Brand", type: "select", options: ["feishu", "lark"] },
    ]},
    { id: "dingtalk", icon: "DT", name: zh ? "\u9489\u9489" : "DingTalk", fields: [
      { key: "appKey", label: "App Key", type: "text", placeholder: "" },
      { key: "appSecret", label: "App Secret", type: "password", placeholder: "${DINGTALK_APP_SECRET}" },
    ]},
    { id: "telegram", icon: "Tg", name: "Telegram", fields: [
      { key: "botToken", label: "Bot Token", type: "password", placeholder: "${TELEGRAM_BOT_TOKEN}" },
    ]},
    { id: "discord", icon: "Dc", name: "Discord", fields: [
      { key: "token", label: "Bot Token", type: "password", placeholder: "${DISCORD_BOT_TOKEN}" },
    ]},
    { id: "slack", icon: "Sl", name: "Slack", fields: [
      { key: "botToken", label: "Bot Token", type: "password", placeholder: "${SLACK_BOT_TOKEN}" },
      { key: "appToken", label: "App Token", type: "password", placeholder: "${SLACK_APP_TOKEN}" },
    ]},
    { id: "whatsapp", icon: "WA", name: "WhatsApp", fields: [
      { key: "phoneNumberId", label: "Phone Number ID", type: "text", placeholder: "" },
      { key: "accessToken", label: "Access Token", type: "password", placeholder: "${WHATSAPP_TOKEN}" },
    ]},
    { id: "qq", icon: "QQ", name: "QQ", fields: [
      { key: "appId", label: "App ID", type: "text", placeholder: "" },
      { key: "appSecret", label: "App Secret", type: "password", placeholder: "${QQ_APP_SECRET}" },
    ]},
    { id: "line", icon: "Li", name: "LINE", fields: [
      { key: "channelSecret", label: "Channel Secret", type: "password", placeholder: "${LINE_CHANNEL_SECRET}" },
      { key: "channelAccessToken", label: "Access Token", type: "password", placeholder: "${LINE_ACCESS_TOKEN}" },
    ]},
    { id: "zalo", icon: "Za", name: "Zalo", fields: [
      { key: "appId", label: "App ID", type: "text", placeholder: "" },
      { key: "accessToken", label: "Access Token", type: "password", placeholder: "${ZALO_ACCESS_TOKEN}" },
    ]},
    { id: "matrix", icon: "Mx", name: "Matrix", fields: [
      { key: "homeserver", label: "Homeserver", type: "text", placeholder: "https://matrix.org" },
      { key: "userId", label: "User ID", type: "text", placeholder: "@bot:matrix.org" },
      { key: "accessToken", label: "Access Token", type: "password", placeholder: "${MATRIX_ACCESS_TOKEN}" },
    ]},
    { id: "signal", icon: "Sg", name: "Signal", fields: [
      { key: "phoneNumber", label: "Phone Number", type: "text", placeholder: "+1234567890" },
    ]},
  ];

  const POLICY_OPTIONS = ["pairing", "open", "allowlist", "disabled"];

  // Channel config state: { [channelId]: { enabled, expanded, fields: {key: value}, dmPolicy, groupPolicy } }
  const [channelConfigs, setChannelConfigs] = useState<Record<string, {
    enabled: boolean; expanded: boolean; fields: Record<string, string>; dmPolicy: string; groupPolicy: string;
  }>>(() => {
    const init: any = {};
    ALL_CHANNELS_DEF.forEach((ch) => {
      // Check if channel exists in parsed config
      const existing = channels.find((c) => c.type === ch.id);
      init[ch.id] = {
        enabled: !!existing?.enabled,
        expanded: false,
        fields: {},
        dmPolicy: "pairing",
        groupPolicy: "allowlist",
      };
    });
    return init;
  });

  // Sync from parsed config on load
  useEffect(() => {
    if (!rawConfig) return;
    const next = { ...channelConfigs };
    ALL_CHANNELS_DEF.forEach((chDef) => {
      // Try to extract channel block from raw config
      const blockRe = new RegExp(`["']?${chDef.id}["']?\\s*:\\s*\\{([^}]*)\\}`, "ms");
      const m = rawConfig.match(blockRe);
      if (m) {
        next[chDef.id].enabled = true;
        const block = m[1];
        chDef.fields.forEach((f) => {
          const valRe = new RegExp(`["']?${f.key}["']?\\s*:\\s*["']([^"']*)["']`);
          const vm = block.match(valRe);
          if (vm) next[chDef.id].fields[f.key] = vm[1];
        });
        const dmRe = /["']?dmPolicy["']?\s*:\s*["']([^"']*)["']/;
        const dm = block.match(dmRe);
        if (dm) next[chDef.id].dmPolicy = dm[1];
        const gpRe = /["']?groupPolicy["']?\s*:\s*["']([^"']*)["']/;
        const gp = block.match(gpRe);
        if (gp) next[chDef.id].groupPolicy = gp[1];
      }
    });
    setChannelConfigs(next);
  }, [rawConfig]);

  const updateChannelField = (chId: string, key: string, value: string) => {
    setChannelConfigs((prev) => ({
      ...prev,
      [chId]: { ...prev[chId], fields: { ...prev[chId].fields, [key]: value } },
    }));
    markDirty();
  };

  const toggleChannelEnabled = (chId: string) => {
    setChannelConfigs((prev) => ({
      ...prev,
      [chId]: { ...prev[chId], enabled: !prev[chId].enabled },
    }));
    markDirty();
  };

  const toggleChannelExpanded = (chId: string) => {
    setChannelConfigs((prev) => ({
      ...prev,
      [chId]: { ...prev[chId], expanded: !prev[chId].expanded },
    }));
  };

  const chIconStyle = (on: boolean) => ({
    width: 32, height: 32, borderRadius: 8, display: "flex", alignItems: "center", justifyContent: "center",
    fontSize: 11, fontWeight: 700, flexShrink: 0 as const,
    background: on ? "rgba(249,115,22,0.15)" : "#1a1c20",
    color: on ? "#f97316" : "#2e2c3a",
    border: `1px solid ${on ? "rgba(249,115,22,0.25)" : "#252830"}`,
  });

  const renderChannels = () => (
    <div>
      <div style={{ fontSize: 11, color: "#2a2836", marginBottom: 12 }}>
        {zh ? "\u70B9\u51FB\u901A\u9053\u53F3\u4FA7\u5F00\u5173\u542F\u7528\uFF0C\u5C55\u5F00\u586B\u5199\u51ED\u8BC1\uFF0C\u4FDD\u5B58\u540E\u751F\u6548\u3002" : "Toggle channels on, expand to fill credentials, save to apply."}
      </div>
      <div style={sectionCard}>
        {ALL_CHANNELS_DEF.map((chDef) => {
          const cc = channelConfigs[chDef.id] || { enabled: false, expanded: false, fields: {}, dmPolicy: "pairing", groupPolicy: "allowlist" };
          return (
            <div key={chDef.id} style={{ borderBottom: "1px solid #111315" }}>
              <div style={{ display: "flex", alignItems: "center", gap: 12, padding: "11px 14px", cursor: "pointer" }}
                onClick={() => toggleChannelExpanded(chDef.id)}>
                <div style={chIconStyle(cc.enabled)}>{chDef.icon}</div>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ fontSize: 12, fontWeight: 500, color: cc.enabled ? "#c8c6d4" : "#6a6878" }}>{chDef.name}</div>
                  <div style={{ fontSize: 10, color: "#252530", fontFamily: "'JetBrains Mono', monospace", marginTop: 1 }}>
                    {cc.enabled && Object.values(cc.fields).some(Boolean) ? chDef.id + " \u00B7 " + cc.dmPolicy : (zh ? "\u672A\u914D\u7F6E" : "Not configured")}
                  </div>
                </div>
                <span style={statusPill(cc.enabled ? (Object.values(cc.fields).some(Boolean) ? "configured" : "pending") : "disabled")}>
                  {cc.enabled
                    ? (Object.values(cc.fields).some(Boolean) ? (zh ? "\u5DF2\u914D\u7F6E" : "Configured") : (zh ? "\u5F85\u914D\u7F6E" : "Pending"))
                    : (zh ? "\u672A\u542F\u7528" : "disabled")}
                </span>
                <div onClick={(e) => e.stopPropagation()}>
                  <Toggle on={cc.enabled} onToggle={() => toggleChannelEnabled(chDef.id)} />
                </div>
                <span style={{ fontSize: 10, color: "#3e3c4a", transition: "transform 0.15s", transform: cc.expanded ? "rotate(90deg)" : "none" }}>{"\u25B6"}</span>
              </div>
              {cc.expanded && (
                <div style={{ padding: "0 14px 14px 56px", display: "flex", flexDirection: "column", gap: 8 }}>
                  {chDef.fields.map((f) => (
                    <div key={f.key}>
                      <div style={{ fontSize: 10, color: "#35323f", marginBottom: 4, fontFamily: "'JetBrains Mono', monospace" }}>{f.label}</div>
                      {f.type === "select" ? (
                        <select style={fieldSelect} value={cc.fields[f.key] || f.options?.[0] || ""}
                          onChange={(e) => updateChannelField(chDef.id, f.key, e.target.value)}>
                          {f.options?.map((o) => <option key={o} value={o}>{o}</option>)}
                        </select>
                      ) : (
                        <input style={fieldInput} type={f.type} value={cc.fields[f.key] || ""}
                          placeholder={f.placeholder}
                          onChange={(e) => updateChannelField(chDef.id, f.key, e.target.value)} />
                      )}
                    </div>
                  ))}
                  <div style={{ display: "flex", gap: 8 }}>
                    <div style={{ flex: 1 }}>
                      <div style={{ fontSize: 10, color: "#35323f", marginBottom: 4 }}>dmPolicy</div>
                      <select style={fieldSelect} value={cc.dmPolicy}
                        onChange={(e) => { setChannelConfigs((p) => ({ ...p, [chDef.id]: { ...p[chDef.id], dmPolicy: e.target.value } })); markDirty(); }}>
                        {POLICY_OPTIONS.map((o) => <option key={o} value={o}>{o}</option>)}
                      </select>
                    </div>
                    <div style={{ flex: 1 }}>
                      <div style={{ fontSize: 10, color: "#35323f", marginBottom: 4 }}>groupPolicy</div>
                      <select style={fieldSelect} value={cc.groupPolicy}
                        onChange={(e) => { setChannelConfigs((p) => ({ ...p, [chDef.id]: { ...p[chDef.id], groupPolicy: e.target.value } })); markDirty(); }}>
                        {POLICY_OPTIONS.map((o) => <option key={o} value={o}>{o}</option>)}
                      </select>
                    </div>
                  </div>
                </div>
              )}
            </div>
          );
        })}
      </div>
    </div>
  );

  const renderTools = () => (
    <div>
      <div style={sectionCard}>
        <div style={sectionTitle}>{getLang() === "cn" ? "\u6267\u884C\u5DE5\u5177" : "Exec Tool"}</div>
        <div style={{ ...fieldRow, borderBottom: "none" }}>
          <div>
            <div style={fieldLabel}>{getLang() === "cn" ? "\u6C99\u7BB1\u6A21\u5F0F" : "Sandbox Mode"}</div>
            <div style={fieldSub}>{getLang() === "cn" ? "\u9650\u5236\u4EE3\u7801\u6267\u884C\u73AF\u5883" : "Restrict code execution environment"}</div>
          </div>
          <Toggle on={execSandbox} onToggle={() => { setExecSandbox(!execSandbox); markDirty(); }} />
        </div>
      </div>
      <div style={sectionCard}>
        <div style={sectionTitle}>{getLang() === "cn" ? "\u4E0A\u4F20\u9650\u5236" : "Upload Limits"}</div>
        <div style={{ ...fieldRow, borderBottom: "none" }}>
          <div>
            <div style={fieldLabel}>{getLang() === "cn" ? "\u6700\u5927\u4E0A\u4F20\u5927\u5C0F (MB)" : "Max Upload Size (MB)"}</div>
          </div>
          <input style={fieldInput} type="number" value={uploadMaxSize}
            onChange={(e) => { setUploadMaxSize(parseInt(e.target.value, 10) || 10); markDirty(); }} />
        </div>
      </div>
      <div style={sectionCard}>
        <div style={sectionTitle}>{getLang() === "cn" ? "\u7F51\u7EDC\u641C\u7D22" : "Web Search"}</div>
        <div style={{ ...fieldRow, borderBottom: "none" }}>
          <div>
            <div style={fieldLabel}>{getLang() === "cn" ? "\u641C\u7D22\u63D0\u4F9B\u5546" : "Search Provider"}</div>
          </div>
          <select style={fieldSelect} value={webSearchProvider}
            onChange={(e) => { setWebSearchProvider(e.target.value); markDirty(); }}>
            <option value="none">{getLang() === "cn" ? "\u5173\u95ED" : "None"}</option>
            <option value="tavily">Tavily</option>
            <option value="searxng">SearXNG</option>
            <option value="bing">Bing</option>
          </select>
        </div>
      </div>
      <div style={sectionCard}>
        <div style={sectionTitle}>{getLang() === "cn" ? "\u8BB0\u5FC6\u7BA1\u7406" : "Memory"}</div>
        <div style={fieldRow}>
          <div>
            <div style={fieldLabel}>{getLang() === "cn" ? "\u77ED\u671F\u8BB0\u5FC6\u4E0A\u9650" : "Short-term Limit"}</div>
          </div>
          <div style={sliderContainer}>
            <input type="range" min={5} max={50} value={memoryShortTermLimit}
              style={{ flex: 1, accentColor: "#f0a500" }}
              onChange={(e) => { setMemoryShortTermLimit(parseInt(e.target.value, 10)); markDirty(); }} />
            <span style={{ fontSize: "13px", fontWeight: 500, minWidth: "30px", textAlign: "right" }}>{memoryShortTermLimit}</span>
          </div>
        </div>
        <div style={{ ...fieldRow, borderBottom: "none" }}>
          <div>
            <div style={fieldLabel}>{getLang() === "cn" ? "\u957F\u671F\u8BB0\u5FC6\u4E0A\u9650" : "Long-term Limit"}</div>
          </div>
          <div style={sliderContainer}>
            <input type="range" min={10} max={500} value={memoryLongTermLimit}
              style={{ flex: 1, accentColor: "#f0a500" }}
              onChange={(e) => { setMemoryLongTermLimit(parseInt(e.target.value, 10)); markDirty(); }} />
            <span style={{ fontSize: "13px", fontWeight: 500, minWidth: "30px", textAlign: "right" }}>{memoryLongTermLimit}</span>
          </div>
        </div>
      </div>
    </div>
  );

  const renderRawEditor = () => (
    <div>
      <div className={styles["ws-editor"]} style={{ height: "calc(100vh - 48px - 200px)" }}>
        <textarea
          className={styles["ws-textarea"]}
          value={rawConfig}
          onChange={(e) => { setRawConfig(e.target.value); setDirty(true); }}
          spellCheck={false}
        />
      </div>
      <div className={styles["note"] + " " + styles["info"]} style={{ marginTop: "12px" }}>
        <span>i</span>
        <span>{Locale.RsClawPanel.Config.ReloadNote}</span>
      </div>
    </div>
  );

  // Debug: catch render errors
  try {
    // test all locale keys to find undefined ones
    const _test = [
      Locale.RsClawPanel.Config.PageTitle,
      Locale.RsClawPanel.Config.Gateway,
      Locale.RsClawPanel.Config.Port,
      Locale.RsClawPanel.Config.Bind,
      Locale.RsClawPanel.Config.BindLoopback,
      Locale.RsClawPanel.Config.BindAll,
      Locale.RsClawPanel.Config.Language,
      Locale.RsClawPanel.Config.ProcessingTimeout,
      Locale.RsClawPanel.Config.SaveAndReload,
      Locale.RsClawPanel.Config.Saving,
      Locale.RsClawPanel.Config.ReloadNote,
    ];
    const missing = _test.findIndex(v => v === undefined);
    if (missing >= 0) console.error("[Config] Missing locale key at index", missing);
  } catch (e) {
    return <div style={{padding:20,color:"#d95f5f"}}>Config render error: {String(e)}</div>;
  }

  return (
    <div>
      <div className={styles["page-header"]}>
        <div>
          <div className={styles["page-title"]}>{Locale.RsClawPanel.Config.PageTitle}</div>
          <div className={styles["page-sub"]}>{configPath}</div>
        </div>
      </div>

      {/* Tab bar + action buttons */}
      <div style={{
        display: "flex", alignItems: "center", justifyContent: "space-between",
        borderBottom: "1px solid var(--border-in-light)", marginBottom: "16px", paddingBottom: "0",
      }}>
        <div style={{ display: "flex", gap: "0" }}>
          {!rawMode && tabs.map((tab) => (
            <button key={tab.key}
              onClick={() => setActiveTab(tab.key)}
              style={{
                padding: "10px 18px", fontSize: "13px", fontWeight: activeTab === tab.key ? 600 : 400,
                color: activeTab === tab.key ? "#f0a500" : "var(--black)",
                background: "transparent", border: "none", cursor: "pointer",
                borderBottom: activeTab === tab.key ? "2px solid #f0a500" : "2px solid transparent",
                transition: "all 0.15s", marginBottom: "-1px",
              }}>
              {tab.label}
            </button>
          ))}
          {rawMode && (
            <div style={{ padding: "10px 18px", fontSize: "13px", fontWeight: 600, color: "#f0a500" }}>
              {getLang() === "cn" ? "\u539F\u59CB JSON5" : "Raw JSON5"}
            </div>
          )}
        </div>
        <div style={{ display: "flex", gap: "8px", paddingBottom: "8px" }}>
          <button
            className={styles["btn"]}
            onClick={() => {
              if (rawMode) {
                // Switching back to structured - re-parse
                parseConfig(rawConfig);
              }
              setRawMode(!rawMode);
            }}
            style={{ fontSize: "12px" }}
          >
            {rawMode
              ? (getLang() === "cn" ? "\u8FD4\u56DE\u7ED3\u6784\u5316\u7F16\u8F91" : "Back to Structured")
              : (getLang() === "cn" ? "\u67E5\u770B\u539F\u59CB JSON5" : "View Raw JSON5")}
          </button>
          <button
            className={`${styles["btn"]} ${styles["primary"]}`}
            onClick={handleSave}
            disabled={saving || !dirty}
            style={{ fontSize: "12px" }}
          >
            {saving ? Locale.RsClawPanel.Config.Saving : Locale.RsClawPanel.Config.SaveAndReload}
          </button>
        </div>
      </div>

      {/* Tab content */}
      <div style={{ overflowY: "auto", maxHeight: "calc(100vh - 48px - 200px)", paddingRight: "4px" }}>
        {rawMode ? renderRawEditor()
          : activeTab === "gateway" ? renderGateway()
          : activeTab === "models" ? renderModels()
          : activeTab === "channels" ? renderChannels()
          : renderTools()}
      </div>
    </div>
  );
}

// ══════════════════════════════════════════════════════════
// ── Agent Workspace Editor (embedded in Agent Manager) ───
// ══════════════════════════════════════════════════════════

function AgentWorkspaceEditor({ agentId }: { agentId: string }) {
  const [files, setFiles] = useState<string[]>([]);
  const [activeFile, setActiveFile] = useState("");
  const [content, setContent] = useState("");
  const [saving, setSaving] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [showNewFile, setShowNewFile] = useState(false);
  const [newFileName, setNewFileName] = useState("");
  const [showTemplates, setShowTemplates] = useState(false);

  const applyTemplate = async (tpl: AgentTemplate) => {
    for (const [fileName, fileContent] of Object.entries(tpl.files)) {
      await writeWorkspaceFile(fileName, fileContent, agentId);
    }
    toast.success(Locale.RsClawPanel.Agents.TemplateApplied);
    setShowTemplates(false);
    fetchFiles();
  };

  const fetchFiles = useCallback(async () => {
    try {
      const data = await listWorkspaceFiles(agentId);
      setFiles(data.files || []);
      if (data.files?.length > 0) loadFile(data.files[0]);
    } catch { setFiles([]); }
  }, [agentId]);

  const loadFile = async (name: string) => {
    try {
      const data = await readWorkspaceFile(name, agentId);
      setContent(data.content || "");
      setActiveFile(name);
      setDirty(false);
    } catch { setContent(""); }
  };

  const handleSave = async () => {
    if (!activeFile) return;
    setSaving(true);
    try {
      await writeWorkspaceFile(activeFile, content, agentId);
      setDirty(false);
      toast.success(Locale.RsClawPanel.Workspace.SaveSuccess);
    } catch (e) {
      toast.fromError(Locale.RsClawPanel.Workspace.SaveFailed, e);
    } finally { setSaving(false); }
  };

  const handleCreateFile = async () => {
    let name = newFileName.trim();
    if (!name) return;
    if (!name.endsWith(".md")) name += ".md";
    try {
      await writeWorkspaceFile(name, `# ${name}\n\n`, agentId);
      setShowNewFile(false);
      setNewFileName("");
      await fetchFiles();
      loadFile(name);
    } catch (e) {
      toast.fromError(Locale.RsClawPanel.Workspace.SaveFailed, e);
    }
  };

  useEffect(() => { fetchFiles(); }, [fetchFiles]);

  return (
    <div>
      <div style={{ display: "flex", justifyContent: "flex-end", marginBottom: "12px", gap: "8px" }}>
        <button
          className={styles["btn"]}
          onClick={() => setShowTemplates(!showTemplates)}
        >
          {Locale.RsClawPanel.Agents.Templates}
        </button>
        <button
          className={`${styles["btn"]} ${styles["primary"]}`}
          onClick={handleSave}
          disabled={saving || !dirty}
        >
          {saving ? Locale.RsClawPanel.Config.Saving : Locale.RsClawPanel.Workspace.Save}
        </button>
      </div>
      {showTemplates && (
        <div className={styles["provider-grid"]} style={{ marginBottom: "12px" }}>
          {AGENT_TEMPLATES.map((tpl) => (
            <div
              key={tpl.id}
              className={styles["prov-card"]}
              onClick={() => applyTemplate(tpl)}
              style={{ cursor: "pointer" }}
            >
              <div className={styles["prov-logo"]}>
                <EmojiAvatar avatar={tpl.icon} size={14} />
              </div>
              <div>
                <div className={styles["prov-n"]}>{tpl.name}</div>
                <div className={styles["prov-s"]}>{tpl.desc}</div>
              </div>
            </div>
          ))}
        </div>
      )}
      <div className={styles["ws-layout"]}>
        <div className={styles["ws-file-list"]}>
          {files.map((f) => (
            <button
              key={f}
              className={`${styles["ws-file-item"]} ${f === activeFile ? styles["active"] : ""}`}
              onClick={() => loadFile(f)}
            >{f}</button>
          ))}
          {showNewFile ? (
            <div className={styles["ws-new-file"]}>
              <input
                autoFocus
                className={styles["cfg-input"]}
                value={newFileName}
                placeholder={Locale.RsClawPanel.Workspace.NewFilePlaceholder}
                onChange={(e) => setNewFileName(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter") handleCreateFile(); }}
                style={{ fontSize: "11px", padding: "5px 8px" }}
              />
              <div style={{ display: "flex", gap: "4px", marginTop: "4px" }}>
                <button className={styles["btn"]} onClick={() => { setShowNewFile(false); setNewFileName(""); }} style={{ fontSize: "10px", padding: "3px 8px" }}>
                  {Locale.RsClawPanel.Workspace.Cancel}
                </button>
                <button className={`${styles["btn"]} ${styles["primary"]}`} onClick={handleCreateFile} style={{ fontSize: "10px", padding: "3px 8px" }}>
                  {Locale.RsClawPanel.Workspace.Create}
                </button>
              </div>
            </div>
          ) : (
            <button className={styles["ws-file-item"]} onClick={() => setShowNewFile(true)} style={{ color: "#f97316" }}>
              + {Locale.RsClawPanel.Workspace.NewFile}
            </button>
          )}
        </div>
        <div className={styles["ws-editor"]}>
          <textarea
            className={styles["ws-textarea"]}
            value={content}
            onChange={(e) => { setContent(e.target.value); setDirty(true); }}
            placeholder={Locale.RsClawPanel.Workspace.EditorPlaceholder}
            spellCheck={false}
          />
        </div>
      </div>
    </div>
  );
}

// ══════════════════════════════════════════════════════════
// ── Agent Manager Page ───────────────────────────────────
// ══════════════════════════════════════════════════════════
// ── Agent Templates ──────────────────────────────────────

interface AgentTemplate {
  id: string;
  icon: string;
  name: string;
  desc: string;
  files: Record<string, string>;
}

function getAgentTemplates(): AgentTemplate[] {
  const zh = getLang() === "cn";
  return [
    {
      id: "assistant",
      icon: "1f916",
      name: zh ? "AI 助手" : "AI Assistant",
      desc: zh ? "通用 AI 助手，拥有全部工具权限" : "General-purpose AI assistant with full tool access",
      files: {
        "SOUL.md": zh
          ? "# SOUL.md\n\n你是一个能干的 AI 助手。\n\n## 性格\n- 直接、有帮助\n- 聊天时简洁，需要时详尽\n- 坦诚承认不确定的事\n\n## 权限\n- 可使用所有工具\n- 无需请求许可，直接行动\n"
          : "# SOUL.md\n\nYou are a capable AI assistant.\n\n## Personality\n- Direct and helpful\n- Concise in chat, thorough when it matters\n- Admit uncertainty openly\n\n## Permissions\n- Full access to all available tools\n- No need to ask for permission before using tools\n",
        "AGENTS.md": zh
          ? "# AGENTS.md\n\n## 规则\n- 主动解决问题，减少反问\n- 积极使用工具\n- 保持回答聚焦、可操作\n"
          : "# AGENTS.md\n\nStanding instructions for this agent.\n\n## Rules\n- Be resourceful: try to solve problems before asking\n- Use tools proactively when they help\n- Keep responses focused and actionable\n",
      },
    },
    {
      id: "coder",
      icon: "1f4bb",
      name: zh ? "编程助手" : "Code Assistant",
      desc: zh ? "软件开发专家" : "Software development specialist",
      files: {
        "SOUL.md": zh
          ? "# SOUL.md\n\n你是一名资深软件工程师。\n\n## 性格\n- 精确、注重细节\n- 用代码说话，少说废话\n- 遵循最佳实践\n\n## 权限\n- 可执行命令、读写文件\n- 可按需安装依赖\n"
          : "# SOUL.md\n\nYou are an expert software engineer.\n\n## Personality\n- Precise and detail-oriented\n- Prefer working code over explanations\n- Follow best practices and conventions\n\n## Permissions\n- Full access to exec, read, write tools\n- Can install packages when needed\n",
        "AGENTS.md": zh
          ? "# AGENTS.md\n\n## 规则\n- 代码整洁可读\n- 生产代码加错误处理\n- 遵循项目现有风格\n- 提交前测试\n- 小步提交\n"
          : "# AGENTS.md\n\n## Rules\n- Write clean, readable code\n- Add error handling for production code\n- Use the project's existing patterns and conventions\n- Test changes before committing\n- Prefer small, focused commits\n",
      },
    },
    {
      id: "researcher",
      icon: "1f50d",
      name: zh ? "研究分析师" : "Research Analyst",
      desc: zh ? "深度研究与分析，支持联网搜索" : "Deep research and analysis with web search",
      files: {
        "SOUL.md": zh
          ? "# SOUL.md\n\n你是一名研究分析师，擅长深度分析。\n\n## 性格\n- 严谨、有条理\n- 基于证据推理\n- 清晰引用来源\n\n## 权限\n- 可使用搜索和阅读工具\n- 可浏览和分析网页内容\n"
          : "# SOUL.md\n\nYou are a research analyst with deep analytical skills.\n\n## Personality\n- Thorough and methodical\n- Evidence-based reasoning\n- Clear citations and sources\n\n## Permissions\n- Full access to web_search and read tools\n- Can browse and analyze web content\n",
        "AGENTS.md": zh
          ? "# AGENTS.md\n\n## 规则\n- 必须标注来源\n- 交叉验证多个来源\n- 区分事实与观点\n- 结构化呈现结果\n- 标注不确定性\n"
          : "# AGENTS.md\n\n## Rules\n- Always cite sources\n- Cross-reference multiple sources\n- Distinguish facts from opinions\n- Present findings in structured format\n- Flag uncertainties and limitations\n",
      },
    },
    {
      id: "writer",
      icon: "270d-fe0f",
      name: zh ? "内容写作" : "Content Writer",
      desc: zh ? "写作、编辑与内容创作" : "Writing, editing, and content creation",
      files: {
        "SOUL.md": zh
          ? "# SOUL.md\n\n你是一名专业的内容写作者和编辑。\n\n## 性格\n- 创意与精确并存\n- 根据受众调整语调\n- 语法和文风功底扎实\n"
          : "# SOUL.md\n\nYou are a professional content writer and editor.\n\n## Personality\n- Creative yet precise\n- Adapts tone to audience\n- Strong grasp of grammar and style\n",
        "AGENTS.md": zh
          ? "# AGENTS.md\n\n## 规则\n- 匹配要求的语调和风格\n- 用清晰、吸引人的语言\n- 用标题和段落组织内容\n- 检查语法和清晰度\n"
          : "# AGENTS.md\n\n## Rules\n- Match the requested tone and style\n- Use clear, engaging language\n- Structure content with headings and paragraphs\n- Proofread for grammar and clarity\n",
      },
    },
    {
      id: "translator",
      icon: "1f30d",
      name: zh ? "翻译专家" : "Translator",
      desc: zh ? "多语言翻译专家" : "Multi-language translation specialist",
      files: {
        "SOUL.md": zh
          ? "# SOUL.md\n\n你是一名精通多语言的翻译专家。\n\n## 性格\n- 注重细微差别和语境\n- 保留原文的语调和意图\n- 有跨文化意识\n"
          : "# SOUL.md\n\nYou are an expert translator fluent in multiple languages.\n\n## Personality\n- Precise with nuance and context\n- Preserves tone and intent\n- Culturally aware\n",
        "AGENTS.md": zh
          ? "# AGENTS.md\n\n## 规则\n- 保留原文含义和语调\n- 恰当改编习语\n- 必要时注明文化差异\n- 遇到歧义术语主动询问\n"
          : "# AGENTS.md\n\n## Rules\n- Preserve the original meaning and tone\n- Adapt idioms appropriately\n- Note cultural differences when relevant\n- Ask for clarification on ambiguous terms\n",
      },
    },
    {
      id: "customer_support",
      icon: "1f4de",
      name: zh ? "客户支持" : "Customer Support",
      desc: zh ? "客户服务与技术支持" : "Customer service and support agent",
      files: {
        "SOUL.md": zh
          ? "# SOUL.md\n\n你是一名友善、专业的客户支持人员。\n\n## 性格\n- 耐心、有同理心\n- 以解决问题为导向\n- 解释清晰简单\n"
          : "# SOUL.md\n\nYou are a friendly and professional customer support agent.\n\n## Personality\n- Patient and empathetic\n- Solution-oriented\n- Clear and simple explanations\n",
        "AGENTS.md": zh
          ? "# AGENTS.md\n\n## 规则\n- 先确认客户的问题\n- 提供分步解决方案\n- 无法解决时升级处理\n- 跟进确保满意\n"
          : "# AGENTS.md\n\n## Rules\n- Always acknowledge the customer's concern\n- Provide step-by-step solutions\n- Escalate when unable to resolve\n- Follow up to ensure satisfaction\n",
      },
    },
  ];
}

const AGENT_TEMPLATES = getAgentTemplates();

function AgentManagerPage() {
  const [agentList, setAgentList] = useState<AgentInfo[]>([]);
  const [showModal, setShowModal] = useState(false);
  const [editAgent, setEditAgent] = useState<AgentInfo | null>(null);
  const [wsAgentId, setWsAgentId] = useState<string | null>(null);
  const [newId, setNewId] = useState("");
  const [newName, setNewName] = useState("");
  const [newAvatar, setNewAvatar] = useState("");
  const [showAvatarPicker, setShowAvatarPicker] = useState(false);
  const [newModel, setNewModel] = useState("");
  const [newChannels, setNewChannels] = useState<string[]>([]);
  const [newToolset, setNewToolset] = useState("full");
  const [newSystem, setNewSystem] = useState("");
  const [idError, setIdError] = useState("");
  const [channelAccounts, setChannelAccounts] = useState<Record<string, string[]>>({});
  const [configModels, setConfigModels] = useState<string[]>([]);
  const [configProviders, setConfigProviders] = useState<{ id: string; hasKey: boolean }[]>([]);
  const [selectedProvider, setSelectedProvider] = useState("");
  const [providerModels, setProviderModels] = useState<string[]>([]);
  const [loadingModels, setLoadingModels] = useState(false);

  const fetchAgentList = useCallback(async () => {
    // Read directly from config file (no gateway API dependency)
    const invoke = (window as any).__TAURI__?.invoke;
    if (!invoke) return;
    try {
      const raw: string = await invoke("read_config_file");
      const cfg = JSON.parse(raw || "{}");
      // Auto-fix legacy string model fields in config
      let needsWrite = false;
      for (const a of (cfg.agents?.list || [])) {
        if (typeof a.model === "string") {
          a.model = a.model ? { primary: a.model } : undefined;
          needsWrite = true;
        }
      }
      if (needsWrite) {
        try { await invoke("write_config", { content: JSON.stringify(cfg, null, 2) }); } catch {}
      }
      const list = (cfg.agents?.list || []).map((a: any) => {
        const rawModel = a.model;
        const modelStr = typeof rawModel === "string" ? rawModel : rawModel?.primary || "";
        return { ...a, model: modelStr, status: a.status || "idle" };
      });
      setAgentList(list);
    } catch {}
  }, []);

  useEffect(() => {
    fetchAgentList();
    // Load channel accounts and config models
    (async () => {
      try {
        const invoke = (window as any).__TAURI__?.invoke;
        if (invoke) {
          const accts: Record<string, string[]> = await invoke("get_channel_accounts");
          setChannelAccounts(accts || {});
          // Read config to extract providers and model aliases
          try {
            const raw: string = await invoke("read_config_file");
            const cfg = JSON.parse(raw || "{}");
            const models: string[] = [];
            const providers: { id: string; hasKey: boolean }[] = [];
            // Add model aliases from agents.defaults.models
            const defaults = cfg?.agents?.defaults?.models;
            if (defaults && typeof defaults === "object") {
              Object.keys(defaults).forEach((alias) => {
                const val = defaults[alias];
                models.push(typeof val === "string" ? val : val?.model || alias);
              });
            }
            // Extract providers with their API key status
            const provs = cfg?.models?.providers;
            if (provs && typeof provs === "object") {
              Object.keys(provs).forEach((provName) => {
                const provConf = provs[provName];
                const hasKey = !!(provConf?.apiKey || provConf?.baseUrl);
                providers.push({ id: provName, hasKey });
                if (provConf?.models && Array.isArray(provConf.models)) {
                  provConf.models.forEach((m: any) => {
                    const id = typeof m === "string" ? m : m?.id || m?.name;
                    if (id) models.push(`${provName}/${id}`);
                  });
                }
              });
            }
            setConfigModels(models);
            setConfigProviders(providers);
          } catch {}
        }
      } catch {}
    })();
  }, [fetchAgentList]);

  const validateId = (id: string) => {
    if (!id) { setIdError(""); return false; }
    if (!/^[a-zA-Z0-9_-]+$/.test(id)) { setIdError(getLang() === "cn" ? "只允许字母、数字、下划线、短横线" : "Only letters, numbers, _ and - allowed"); return false; }
    if (!editAgent && agentList.some((a) => a.id === id)) { setIdError(getLang() === "cn" ? "ID 已存在" : "ID already exists"); return false; }
    setIdError("");
    return true;
  };

  const handleSaveAgent = async () => {
    if (!newId.trim() || idError) {
      toast.warn(getLang() === "cn" ? "请填写有效的智能体 ID" : "Please enter a valid Agent ID");
      return;
    }
    if (!validateId(newId.trim())) return;
    const agent: Record<string, any> = {
      id: newId.trim(),
      name: newName || undefined,
      avatar: newAvatar || undefined,
      model: newModel ? { primary: newModel, toolset: newToolset } : undefined,
      channels: newChannels.length > 0 ? newChannels : [],
    };
    try {
      const invoke = (window as any).__TAURI__?.invoke;
      if (!invoke) throw new Error("Tauri not available");
      // Write agent config to JSON file
      const raw: string = await invoke("read_config_file");
      const cfg = JSON.parse(raw || "{}");
      if (!cfg.agents) cfg.agents = {};
      if (!cfg.agents.list) cfg.agents.list = [];
      // Migrate any legacy string model fields to { primary: "..." } format
      for (const a of cfg.agents.list) {
        if (typeof a.model === "string") {
          a.model = a.model ? { primary: a.model } : undefined;
        }
      }
      const idx = cfg.agents.list.findIndex((a: any) => a.id === agent.id);
      if (idx >= 0) {
        cfg.agents.list[idx] = { ...cfg.agents.list[idx], ...agent };
      } else {
        cfg.agents.list.push(agent);
      }
      await invoke("write_config", { content: JSON.stringify(cfg, null, 2) });
      // Write SOUL.md to agent workspace if system prompt provided
      if (newSystem.trim()) {
        try {
          await invoke("write_workspace_file", {
            agentId: agent.id,
            fileName: "SOUL.md",
            content: newSystem,
          });
        } catch {}
      }
      try { await reloadConfig(); } catch {}
      // Re-read from config to refresh list
      await fetchAgentList();
      setShowModal(false);
      resetForm();
      toast.success(getLang() === "cn" ? "\u5DF2\u4FDD\u5B58" : "Saved");
    } catch (e) {
      toast.fromError(Locale.RsClawPanel.Agents.SaveFailed, e);
    }
  };

  const [confirmDeleteId, setConfirmDeleteId] = useState<string | null>(null);

  const handleDeleteAgent = async (id: string) => {
    try {
      // Optimistic update: remove from list immediately
      setAgentList((prev) => prev.filter((a) => a.id !== id));
      setConfirmDeleteId(null);
      // Delete from config file via Tauri
      const invoke = (window as any).__TAURI__?.invoke;
      if (invoke) {
        const raw: string = await invoke("read_config_file");
        const cfg = JSON.parse(raw || "{}");
        if (cfg.agents?.list) {
          cfg.agents.list = cfg.agents.list.filter((a: any) => a.id !== id);
          await invoke("write_config", { content: JSON.stringify(cfg, null, 2) });
          try { await reloadConfig(); } catch {}
        }
      }
      toast.success(getLang() === "cn" ? "\u5DF2\u5220\u9664" : "Deleted");
    } catch (e) {
      // Revert optimistic update on failure
      await fetchAgentList();
      toast.fromError(Locale.RsClawPanel.Agents.DeleteFailed, e);
    }
  };

  const [showTemplateChooser, setShowTemplateChooser] = useState(false);

  const openAddModal = () => {
    resetForm();
    setEditAgent(null);
    setShowTemplateChooser(true);
  };

  const selectTemplate = (tpl: AgentTemplate | null) => {
    resetForm();
    setEditAgent(null);
    setShowTemplateChooser(false);
    if (tpl) {
      setNewName(tpl.name);
      setNewAvatar(tpl.icon);
      setNewSystem(Object.values(tpl.files)[0] || "");
    }
    setShowModal(true);
  };

  const openEditModal = async (agent: AgentInfo) => {
    setEditAgent(agent);
    setNewId(agent.id);
    setNewName(agent.name || "");
    setNewAvatar(agent.avatar || "");
    setNewModel(agent.model || "");
    setNewChannels(agent.channels || []);
    setNewToolset(agent.toolset?.[0] || "full");
    setNewSystem("");
    setSelectedProvider("");
    setProviderModels([]);
    setShowModal(true);
    try {
      const invoke = (window as any).__TAURI__?.invoke;
      if (invoke) {
        const raw = await invoke("read_config_file");
        const cfg = JSON.parse(raw || "{}");
        const agentCfg = (cfg.agents?.list || []).find((a: any) => a.id === agent.id);
        if (agentCfg) {
          if (agentCfg.name) setNewName(agentCfg.name);
          if (agentCfg.avatar) setNewAvatar(agentCfg.avatar);
          if (agentCfg.model) {
            const m = agentCfg.model;
            setNewModel(typeof m === "string" ? m : m?.primary || "");
            if (m?.toolset) setNewToolset(m.toolset);
          }
          if (agentCfg.channels) setNewChannels(agentCfg.channels);
          if (agentCfg.toolset?.[0]) setNewToolset(agentCfg.toolset[0]);
        }
        // Read SOUL.md from agent workspace
        try {
          const soul = await invoke("read_workspace_file", { agentId: agent.id, fileName: "SOUL.md" });
          if (soul) setNewSystem(soul);
        } catch {}
        // Set provider from model string (e.g. "openai/gpt-4" -> provider "openai")
        const rawModel = agentCfg?.model;
        const model = typeof rawModel === "string" ? rawModel : rawModel?.primary || agent.model || "";
        if (model.includes("/")) {
          setSelectedProvider(model.split("/")[0]);
        }
        // Refresh providers from config
        const providers: { id: string; hasKey: boolean }[] = [];
        const provs = cfg?.models?.providers;
        if (provs && typeof provs === "object") {
          Object.keys(provs).forEach((provName) => {
            const provConf = provs[provName];
            providers.push({ id: provName, hasKey: !!(provConf?.apiKey || provConf?.baseUrl) });
          });
        }
        if (providers.length > 0) setConfigProviders(providers);
      }
    } catch {}
  };

  const resetForm = () => {
    setNewId("");
    setNewName("");
    setNewAvatar("");
    setNewModel("");
    setNewChannels([]);
    setNewToolset("full");
    setNewSystem("");
    setIdError("");
    setSelectedProvider("");
    setProviderModels([]);
    setLoadingModels(false);
  };

  if (wsAgentId) {
    return (
      <div>
        <div className={styles["page-header"]}>
          <div>
            <div className={styles["page-title"]}>
              {Locale.RsClawPanel.Agents.Workspace}: {agentList.find(a => a.id === wsAgentId)?.name || wsAgentId}
            </div>
          </div>
          <button className={styles["btn"]} onClick={() => setWsAgentId(null)}>
            &larr; {Locale.RsClawPanel.Agents.BackToList}
          </button>
        </div>
        <AgentWorkspaceEditor agentId={wsAgentId} />
      </div>
    );
  }

  return (
    <div>
      <div className={styles["page-header"]}>
        <div>
          <div className={styles["page-title"]}>{Locale.RsClawPanel.Agents.PageTitle}</div>
          <div className={styles["page-sub"]}>
            {Locale.RsClawPanel.Agents.PageSub}
          </div>
        </div>
        <button
          className={`${styles["btn"]} ${styles["primary"]}`}
          onClick={openAddModal}
        >
          {Locale.RsClawPanel.Agents.NewAgent}
        </button>
      </div>

      <div className={styles["note"] + " " + styles["info"]}>
        <span>i</span>
        {Locale.RsClawPanel.Agents.AgentNote}
      </div>

      {agentList.length > 0 ? (
        agentList.map((agent) => (
          <div key={agent.id} className={styles["agent-card"]}>
            <div className={styles["agent-card-header"]}>
              <div className={styles["agent-card-icon"]}>
                {agent.avatar ? <EmojiAvatar avatar={agent.avatar} size={16} /> : (agent.name || agent.id).charAt(0).toUpperCase()}
              </div>
              <div>
                <div className={styles["agent-card-name"]}>{agent.name || agent.id}</div>
                <div className={styles["agent-card-model"]}>{agent.id}{agent.model ? ` / ${agent.model}` : ""}</div>
              </div>
              <div className={styles["agent-card-actions"]}>
                <span
                  className={`${styles["pill"]} ${
                    agent.status === "active" || agent.status === "idle"
                      ? styles["on"]
                      : styles["off"]
                  }`}
                >
                  {agent.status}
                </span>
                <button
                  className={styles["btn"]}
                  onClick={() => setWsAgentId(agent.id)}
                >
                  {Locale.RsClawPanel.Agents.Workspace}
                </button>
                <button
                  className={styles["btn"]}
                  onClick={() => openEditModal(agent)}
                >
                  {Locale.RsClawPanel.Agents.Edit}
                </button>
                <div style={{ position: "relative", display: "inline-block" }}>
                  <button
                    className={`${styles["btn"]} ${styles["danger"]}`}
                    onClick={() => setConfirmDeleteId(confirmDeleteId === agent.id ? null : agent.id)}
                  >
                    {Locale.RsClawPanel.Agents.Delete}
                  </button>
                  {confirmDeleteId === agent.id && (
                    <div onClick={(e) => e.stopPropagation()} style={{
                      position: "absolute", top: "100%", right: 0, marginTop: 6,
                      padding: "10px 12px", minWidth: 180,
                      background: "var(--white)", border: "1px solid var(--border-in-light)",
                      borderRadius: 8, boxShadow: "0 4px 12px rgba(0,0,0,0.3)", zIndex: 100,
                    }}>
                      <div style={{ fontSize: 11, color: "var(--black)", marginBottom: 8 }}>
                        {getLang() === "cn" ? `\u786E\u8BA4\u5220\u9664 ${agent.name || agent.id}\uFF1F` : `Delete ${agent.name || agent.id}?`}
                      </div>
                      <div style={{ display: "flex", gap: 6, justifyContent: "flex-end" }}>
                        <button onClick={() => setConfirmDeleteId(null)}
                          style={{ fontSize: 10, padding: "3px 10px", borderRadius: 5, border: "1px solid var(--border-in-light)", background: "transparent", color: "var(--black)", cursor: "pointer" }}>
                          {getLang() === "cn" ? "\u53D6\u6D88" : "Cancel"}
                        </button>
                        <button onClick={() => handleDeleteAgent(agent.id)}
                          style={{ fontSize: 10, padding: "3px 10px", borderRadius: 5, border: "none", cursor: "pointer", fontWeight: 600, background: "#d95f5f", color: "#fff" }}>
                          {getLang() === "cn" ? "\u5220\u9664" : "Delete"}
                        </button>
                      </div>
                    </div>
                  )}
                </div>
              </div>
            </div>
            <div className={styles["agent-card-body"]}>
              <div className={styles["agent-card-field"]}>
                <div className={styles["agent-field-label"]}>{Locale.RsClawPanel.Agents.ChannelsLabel}</div>
                <div className={styles["agent-field-value"]}>
                  {agent.channels?.join(", ") || Locale.RsClawPanel.Agents.NoneValue}
                </div>
              </div>
              <div className={styles["agent-card-field"]}>
                <div className={styles["agent-field-label"]}>{Locale.RsClawPanel.Agents.ToolsLabel}</div>
                <div className={styles["agent-field-value"]}>
                  {agent.toolset?.join(", ") || Locale.RsClawPanel.Agents.NoneValue}
                </div>
              </div>
            </div>
          </div>
        ))
      ) : (
        <div className={styles["empty-state"]}>
          {Locale.RsClawPanel.Agents.NoAgents}
        </div>
      )}

      {/* Add/Edit Modal */}
      {/* Template Chooser */}
      {showTemplateChooser && (
        <div className={styles["modal-overlay"]} onClick={() => setShowTemplateChooser(false)}>
          <div className={styles["modal-content"]} onClick={(e) => e.stopPropagation()} style={{ width: 540, maxWidth: "90vw" }}>
            <div className={styles["modal-title"]}>
              {getLang() === "cn" ? "\u9009\u62E9\u667A\u80FD\u4F53\u6A21\u677F" : "Choose Agent Template"}
            </div>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8, marginBottom: 16 }}>
              {AGENT_TEMPLATES.map((tpl) => (
                <div
                  key={tpl.id}
                  onClick={() => selectTemplate(tpl)}
                  style={{
                    padding: "12px 14px", borderRadius: 9, cursor: "pointer",
                    border: "1.5px solid #1e2024", background: "#111315",
                    transition: "all 0.12s", display: "flex", alignItems: "center", gap: 10,
                  }}
                  onMouseEnter={(e) => { e.currentTarget.style.borderColor = "rgba(249,115,22,0.3)"; e.currentTarget.style.background = "rgba(249,115,22,0.05)"; }}
                  onMouseLeave={(e) => { e.currentTarget.style.borderColor = "#1e2024"; e.currentTarget.style.background = "#111315"; }}
                >
                  <div style={{ width: 32, height: 32, borderRadius: 8, background: "rgba(249,115,22,0.12)", display: "flex", alignItems: "center", justifyContent: "center", flexShrink: 0 }}>
                    <EmojiAvatar avatar={tpl.icon} size={16} />
                  </div>
                  <div>
                    <div style={{ fontSize: 12, fontWeight: 600, color: "#c8c6d4" }}>{tpl.name}</div>
                    <div style={{ fontSize: 10, color: "#3e3c4a", marginTop: 2 }}>{tpl.desc}</div>
                  </div>
                </div>
              ))}
            </div>
            <div style={{ display: "flex", justifyContent: "space-between" }}>
              <button className={styles["btn"]} onClick={() => setShowTemplateChooser(false)}>
                {Locale.RsClawPanel.Agents.Cancel}
              </button>
              <button className={styles["btn"]} onClick={() => selectTemplate(null)}>
                {getLang() === "cn" ? "\u7A7A\u767D\u521B\u5EFA" : "Create Blank"}
              </button>
            </div>
          </div>
        </div>
      )}

      {showModal && (
        <div
          className={styles["modal-overlay"]}
          onClick={() => setShowModal(false)}
        >
          <div
            className={styles["modal-content"]}
            onClick={(e) => e.stopPropagation()}
          >
            <div className={styles["modal-title"]}>
              {editAgent ? Locale.RsClawPanel.Agents.EditAgent : Locale.RsClawPanel.Agents.AddAgent}
            </div>

            {/* Agent ID */}
            <div className={styles["cfg-f"]}>
              <div className={styles["cfg-lbl"]}>{Locale.RsClawPanel.Agents.AgentId}</div>
              <input
                className={styles["cfg-input"]}
                value={newId}
                onChange={(e) => { setNewId(e.target.value); validateId(e.target.value); }}
                placeholder="my-agent"
                disabled={!!editAgent}
                style={idError ? { borderColor: "#d95f5f" } : undefined}
              />
              {idError && <div style={{ color: "#d95f5f", fontSize: "10px", marginTop: "4px" }}>{idError}</div>}
            </div>

            {/* Name + Avatar */}
            <div className={styles["cfg-f"]}>
              <div className={styles["cfg-lbl"]}>{Locale.RsClawPanel.Agents.AgentName}</div>
              <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
                <div
                  className={styles["agent-card-icon"]}
                  style={{ cursor: "pointer", flexShrink: 0 }}
                  title={Locale.RsClawPanel.Agents.AvatarHint}
                  onClick={() => setShowAvatarPicker(!showAvatarPicker)}
                >
                  {newAvatar ? <EmojiAvatar avatar={newAvatar} size={16} /> : "?"}
                </div>
                <input
                  className={styles["cfg-input"]}
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                  placeholder={Locale.RsClawPanel.Agents.AgentNamePlaceholder}
                  style={{ flex: 1 }}
                />
              </div>
              {showAvatarPicker && (
                <div style={{ position: "fixed", inset: 0, background: "rgba(0,0,0,0.3)", zIndex: 2000, display: "flex", alignItems: "center", justifyContent: "center" }} onClick={() => setShowAvatarPicker(false)}>
                  <div onClick={(e) => e.stopPropagation()} style={{ width: "350px", maxHeight: "400px", overflow: "auto", borderRadius: "12px" }}>
                    <AvatarPicker onEmojiClick={(emoji) => { setNewAvatar(emoji); setShowAvatarPicker(false); }} />
                  </div>
                </div>
              )}
            </div>

            {/* Model - select provider then pick model */}
            <div className={styles["cfg-f"]}>
              <div className={styles["cfg-lbl"]}>{Locale.RsClawPanel.Agents.Model}</div>
              {/* Provider selector */}
              <select
                className={styles["cfg-select"]}
                value={selectedProvider}
                onChange={async (e) => {
                  const provId = e.target.value;
                  setSelectedProvider(provId);
                  setProviderModels([]);
                  if (!provId) { setNewModel(""); return; }
                  // Fetch models from provider via Tauri (direct client request, no gateway)
                  setLoadingModels(true);
                  try {
                    const invoke = (window as any).__TAURI__?.invoke;
                    if (invoke) {
                      const raw: string = await invoke("read_config_file");
                      const cfg = JSON.parse(raw || "{}");
                      const provConf = cfg?.models?.providers?.[provId] || {};
                      const apiKey = provConf.apiKey || "";
                      const baseUrl = provConf.baseUrl || undefined;
                      const result = await invoke("test_provider", { provider: provId, apiKey, baseUrl });
                      if (result.ok && result.models?.length > 0) {
                        setProviderModels(result.models);
                      }
                    }
                  } catch {} finally { setLoadingModels(false); }
                }}
              >
                <option value="">{getLang() === "cn" ? "-- \u9009\u62E9\u63D0\u4F9B\u5546 --" : "-- Select provider --"}</option>
                {configProviders.map((p) => (
                  <option key={p.id} value={p.id}>{p.id}{!p.hasKey ? (getLang() === "cn" ? " (\u672A\u914D\u7F6E)" : " (not configured)") : ""}</option>
                ))}
              </select>
              {/* Model selector (shown after provider selected) */}
              {selectedProvider && (
                <div style={{ marginTop: "6px" }}>
                  {loadingModels ? (
                    <div style={{ fontSize: 11, color: "#f97316", padding: "6px 0" }}>{getLang() === "cn" ? "\u52A0\u8F7D\u6A21\u578B\u5217\u8868..." : "Loading models..."}</div>
                  ) : providerModels.length > 0 ? (
                    <select
                      className={styles["cfg-select"]}
                      value={newModel}
                      onChange={(e) => setNewModel(e.target.value)}
                    >
                      <option value="">{getLang() === "cn" ? "-- \u9009\u62E9\u6A21\u578B --" : "-- Select model --"}</option>
                      {providerModels.map((m) => (
                        <option key={m} value={`${selectedProvider}/${m}`}>{m}</option>
                      ))}
                    </select>
                  ) : (
                    <div style={{ fontSize: 10, color: "#4a4858", padding: "4px 0" }}>
                      {getLang() === "cn" ? "\u672A\u83B7\u53D6\u5230\u6A21\u578B\uFF0C\u8BF7\u624B\u52A8\u8F93\u5165" : "No models found, enter manually"}
                    </div>
                  )}
                  {/* Manual input always available */}
                  <input
                    className={styles["cfg-input"]}
                    style={{ marginTop: "6px" }}
                    placeholder={`${selectedProvider}/model-name`}
                    value={newModel}
                    onChange={(e) => setNewModel(e.target.value)}
                  />
                </div>
              )}
              {/* Show current model when editing */}
              {!selectedProvider && newModel && (
                <div style={{ fontSize: 11, color: "#a8a6b2", marginTop: 4 }}>
                  {getLang() === "cn" ? "\u5F53\u524D: " : "Current: "}{newModel}
                </div>
              )}
            </div>

            {/* Channels - channel:accountId binding */}
            <div className={styles["cfg-f"]}>
              <div className={styles["cfg-lbl"]}>{Locale.RsClawPanel.Agents.ChannelsLabel} <span style={{ color: "#4a4858", fontSize: "10px" }}>{getLang() === "cn" ? "\u7A7A = \u6240\u6709\u901A\u9053" : "empty = all channels"}</span></div>
              {/* Existing bindings as tags */}
              <div style={{ display: "flex", flexWrap: "wrap", gap: "6px", marginTop: "6px" }}>
                {newChannels.map((ch, i) => (
                  <div key={i} style={{ padding: "4px 10px", borderRadius: "6px", fontSize: "11px", fontWeight: 500, background: "rgba(249,115,22,0.12)", border: "1px solid rgba(249,115,22,0.3)", color: "#f97316", display: "flex", alignItems: "center", gap: "5px" }}>
                    <span style={{ fontFamily: "'JetBrains Mono', monospace" }}>{ch}</span>
                    <span style={{ cursor: "pointer", opacity: 0.6, fontSize: 13 }} onClick={() => setNewChannels((prev) => prev.filter((_, j) => j !== i))}>{"\u2715"}</span>
                  </div>
                ))}
              </div>
              {/* Add binding: dropdown with channel and channel:accountId options */}
              <div style={{ marginTop: "8px" }}>
                <select
                  onChange={(e) => {
                    const val = e.target.value;
                    if (val && !newChannels.includes(val)) setNewChannels((prev) => [...prev, val]);
                    e.target.value = "";
                  }}
                  style={{ padding: "6px 10px", borderRadius: 6, border: "1px solid #252830", background: "#1a1c22", color: "#9896a4", fontSize: 11, outline: "none", width: "100%" }}
                >
                  <option value="">{getLang() === "cn" ? "-- \u9009\u62E9\u901A\u9053 --" : "-- Select channel --"}</option>
                  {(() => {
                    const ALL_CH_IDS = ["wechat", "feishu", "telegram", "dingtalk", "discord", "slack", "wecom", "qq", "whatsapp", "line", "zalo", "matrix", "signal"];
                    const opts: JSX.Element[] = [];
                    for (const chId of ALL_CH_IDS) {
                      const accounts = channelAccounts[chId] || [];
                      const alreadyBound = newChannels.includes(chId);
                      // Always show the bare channel name as an option
                      opts.push(
                        <option key={chId} value={chId} disabled={alreadyBound}>
                          {chId}{alreadyBound ? " \u2713" : ""}
                        </option>
                      );
                      // Also show channel:accountId for channels with accounts
                      for (const acct of accounts) {
                        const binding = `${chId}:${acct}`;
                        const bound = newChannels.includes(binding);
                        opts.push(
                          <option key={binding} value={binding} disabled={bound} style={{ paddingLeft: 12 }}>
                            {"  "}{binding}{bound ? " \u2713" : ""}
                          </option>
                        );
                      }
                    }
                    return opts;
                  })()}
                </select>
              </div>
            </div>

            {/* Toolset - radio cards */}
            <div className={styles["cfg-f"]}>
              <div className={styles["cfg-lbl"]}>{Locale.RsClawPanel.Agents.ToolsLabel}</div>
              <div style={{ display: "flex", gap: "6px", marginTop: "6px" }}>
                {[
                  { value: "minimal", label: "Minimal", desc: getLang() === "cn" ? "6 个核心工具" : "6 core tools" },
                  { value: "standard", label: "Standard", desc: getLang() === "cn" ? "12 个工具" : "12 tools" },
                  { value: "full", label: "Full", desc: getLang() === "cn" ? "全部工具" : "All tools" },
                ].map((opt) => (
                  <div
                    key={opt.value}
                    onClick={() => setNewToolset(opt.value)}
                    style={{
                      flex: 1, padding: "8px 10px", borderRadius: "8px", cursor: "pointer", textAlign: "center",
                      background: newToolset === opt.value ? "rgba(249,115,22,0.08)" : "#1a1c22",
                      border: `1.5px solid ${newToolset === opt.value ? "#f97316" : "#252830"}`,
                      transition: "all 0.12s",
                    }}
                  >
                    <div style={{ fontSize: "12px", fontWeight: 600, color: newToolset === opt.value ? "#f97316" : "#6a6878" }}>{opt.label}</div>
                    <div style={{ fontSize: "9px", color: "#4a4858", marginTop: "2px" }}>{opt.desc}</div>
                  </div>
                ))}
              </div>
            </div>

            <div className={styles["modal-actions"]}>
              <button
                className={styles["btn"]}
                onClick={() => setShowModal(false)}
              >
                {Locale.RsClawPanel.Agents.Cancel}
              </button>
              <button
                className={`${styles["btn"]} ${styles["primary"]}`}
                onClick={handleSaveAgent}
                disabled={!newId.trim() || !!idError}
              >
                {editAgent ? Locale.RsClawPanel.Agents.Update : Locale.RsClawPanel.Agents.Create}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ══════════════════════════════════════════════════════════
// ── Setup Wizard Page ────────────────────────────────────
// ══════════════════════════════════════════════════════════

const PROVIDERS = [
  { id: "anthropic", logo: "Cl", name: "Claude (Anthropic)", desc: "Recommended" },
  { id: "openai", logo: "AI", name: "OpenAI", desc: "GPT-4o" },
  { id: "deepseek", logo: "DS", name: "DeepSeek", desc: "Low cost" },
  { id: "ollama", logo: "Lm", name: "Ollama", desc: "Local model" },
];

function getChannelOptions() {
  const zh = getLang() === "cn";
  return [
    { id: "wechat", icon: "W", name: zh ? "微信" : "WeChat", color: "#07c160" },
    { id: "feishu", icon: "F", name: zh ? "飞书" : "Feishu", color: "#1677ff" },
    { id: "telegram", icon: "Tg", name: "Telegram", color: "#2ca5e0" },
    { id: "dingtalk", icon: "DT", name: zh ? "钉钉" : "DingTalk", color: "#3080f0" },
    { id: "discord", icon: "Dc", name: "Discord", color: "#5865f2" },
    { id: "slack", icon: "Sl", name: "Slack", color: "#4a154b" },
    { id: "wecom", icon: "WC", name: zh ? "企业微信" : "WeCom", color: "#07c160" },
    { id: "matrix", icon: "Mx", name: "Matrix", color: "#000" },
  ];
}
const CHANNEL_OPTIONS = getChannelOptions();

const LANGUAGES = [
  "Chinese", "English", "Japanese", "Korean", "French",
  "German", "Spanish", "Russian", "Arabic", "Portuguese",
];

function SetupWizardPage() {
  const navigate = useNavigate();
  const [step, setStep] = useState(0); // 0=detect, 1=lang, 2=provider, 3=channels, 4=launch
  const [selectedLang, setSelectedLang] = useState("Chinese");
  const [selectedProviders, setSelectedProviders] = useState<string[]>([
    "anthropic",
  ]);
  const [apiKeys, setApiKeys] = useState<Record<string, string>>({});
  const [selectedChannels, setSelectedChannels] = useState<string[]>([]);
  const [launching, setLaunching] = useState(false);
  const [launchChecks, setLaunchChecks] = useState<
    { label: string; status: string }[]
  >([]);
  const [launchDone, setLaunchDone] = useState(false);

  // Step 0: OpenClaw detection
  const [detecting, setDetecting] = useState(true);
  const [openclawPath, setOpenclawPath] = useState<string | null>(null);
  const [migrating, setMigrating] = useState(false);
  const [migrateDone, setMigrateDone] = useState(false);
  const [migrateError, setMigrateError] = useState("");

  // Channel credentials
  const [wechatQrUrl, setWechatQrUrl] = useState("");
  const [wechatQrToken, setWechatQrToken] = useState("");
  const [wechatStatus, setWechatStatus] = useState<"idle" | "scanning" | "connected" | "expired">("idle");
  const [feishuAppId, setFeishuAppId] = useState("");
  const [feishuAppSecret, setFeishuAppSecret] = useState("");

  useEffect(() => {
    (async () => {
      try {
        const tauriInvoke = (window as any).__TAURI__?.invoke;
        if (tauriInvoke) {
          const path = await tauriInvoke("detect_openclaw");
          setOpenclawPath(path || null);
        }
      } catch {}
      setDetecting(false);
    })();
  }, []);

  const handleMigrate = async () => {
    if (!openclawPath) return;
    setMigrating(true);
    setMigrateError("");
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) {
        await tauriInvoke("migrate_openclaw", { sourcePath: openclawPath });
      }
      setMigrateDone(true);
      toast.success(Locale.RsClawPanel.Wizard.MigrateDone);
    } catch (e) {
      setMigrateError((e as Error).message);
      toast.error(Locale.RsClawPanel.Wizard.MigrateFailed);
    } finally {
      setMigrating(false);
    }
  };

  const toggleProvider = (id: string) => {
    setSelectedProviders((prev) =>
      prev.includes(id) ? prev.filter((p) => p !== id) : [...prev, id],
    );
  };

  const toggleChannel = (id: string) => {
    setSelectedChannels((prev) =>
      prev.includes(id) ? prev.filter((c) => c !== id) : [...prev, id],
    );
  };

  // Generate rsclaw.json5 config from wizard selections.
  const generateConfig = () => {
    const providers: Record<string, any> = {};
    for (const p of selectedProviders) {
      if (p === "ollama") {
        providers[p] = { api: "ollama", baseUrl: "http://localhost:11434/v1" };
      } else {
        providers[p] = apiKeys[p] ? { apiKey: apiKeys[p] } : {};
      }
    }
    const channels: Record<string, any> = {};
    for (const ch of selectedChannels) {
      if (ch === "feishu" && feishuAppId) {
        channels[ch] = { appId: feishuAppId, appSecret: feishuAppSecret };
      } else {
        channels[ch] = {};
      }
    }
    const config: any = {
      gateway: { port: 18888, language: selectedLang },
      models: { providers },
      channels,
      agents: { list: [{ id: "main", default: true }] },
    };
    // Pretty-print as JSON (json5 superset of JSON)
    return JSON.stringify(config, null, 2);
  };

  const runLaunch = async () => {
    setLaunching(true);
    const checks = [
      { label: Locale.RsClawPanel.Wizard.CheckGateway, status: "loading" },
      { label: Locale.RsClawPanel.Wizard.CheckHealth, status: "wait" },
      { label: Locale.RsClawPanel.Wizard.CheckChannel, status: "wait" },
      { label: Locale.RsClawPanel.Wizard.CheckModel, status: "wait" },
    ];
    setLaunchChecks([...checks]);

    try {
      // Step 1: Write config (merge with existing to preserve auth token)
      const newConfig = JSON.parse(generateConfig());
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) {
        try { await tauriInvoke("run_setup"); } catch {}
        let existing: any = {};
        try {
          const raw: string = await tauriInvoke("read_config_file");
          existing = JSON.parse(raw || "{}");
        } catch {}
        const merged = { ...newConfig };
        merged.gateway = { ...(existing.gateway || {}), ...(newConfig.gateway || {}) };
        if (existing.gateway?.auth) {
          merged.gateway.auth = existing.gateway.auth;
        }
        const allChannels = { ...(newConfig.channels || {}), ...(existing.channels || {}) };
        for (const [ch, val] of Object.entries(newConfig.channels || {})) {
          if (allChannels[ch] && Object.keys(val as any).length > 0) {
            allChannels[ch] = { ...allChannels[ch], ...(val as any) };
          }
        }
        merged.channels = allChannels;
        await tauriInvoke("write_config", { content: JSON.stringify(merged, null, 2) });
      } else {
        await saveConfig({ raw: JSON.stringify(newConfig, null, 2) });
      }
      checks[0].status = "ok";
      checks[1].status = "loading";
      setLaunchChecks([...checks]);

      // Step 2: Start gateway
      if (tauriInvoke) {
        await tauriInvoke("start_gateway");
      }
      await new Promise((r) => setTimeout(r, 2000));
      checks[1].status = "ok";
      checks[2].status = "loading";
      setLaunchChecks([...checks]);

      // Step 3: Check health
      try {
        await getHealth();
        checks[2].status = "ok";
      } catch {
        checks[2].status = "warn";
      }
      checks[3].status = "loading";
      setLaunchChecks([...checks]);

      // Step 4: Verify model
      await new Promise((r) => setTimeout(r, 500));
      checks[3].status = "ok";
      setLaunchChecks([...checks]);
      setLaunchDone(true);
    } catch (e) {
      // Mark current step as failed
      const current = checks.findIndex((c) => c.status === "loading");
      if (current >= 0) checks[current].status = "error";
      setLaunchChecks([...checks]);
      toast.fromError("Setup failed", e);
    } finally {
      setLaunching(false);
    }
  };

  const stepState = (n: number) => {
    if (n < step) return "done";
    if (n === step) return "active";
    return "";
  };

  return (
    <div>
      <div className={styles["page-title"]} style={{ marginBottom: "4px" }}>
        {Locale.RsClawPanel.Wizard.PageTitle}
      </div>
      <div className={styles["page-sub"]} style={{ marginBottom: "20px" }}>
        {Locale.RsClawPanel.Wizard.PageSub}
      </div>

      {/* Step indicators */}
      <div className={styles["wiz-steps"]}>
        {[0, 1, 2, 3, 4].map((n) => (
          <div
            key={n}
            className={styles["wiz-step"]}
            style={n === 4 ? { flex: 0 } : undefined}
          >
            <div className={`${styles["step-c"]} ${styles[stepState(n)] || ""}`}>
              {n < step ? "\u2713" : n + 1}
            </div>
            {n < 4 && (
              <div
                className={`${styles["step-line"]} ${n < step ? styles["done"] : ""}`}
              />
            )}
          </div>
        ))}
      </div>

      {/* Step 0: Detect OpenClaw */}
      {step === 0 && (
        <div className={styles["wiz-card"]}>
          <div className={styles["wiz-title"]}>{Locale.RsClawPanel.Wizard.DetectTitle}</div>
          <div className={styles["wiz-sub"]}>{Locale.RsClawPanel.Wizard.DetectSub}</div>

          {detecting ? (
            <div className={styles["lcheck"]} style={{ marginBottom: "12px" }}>
              <div className={styles["lcheck-ico"]}>{"\u23F3"}</div>
              <div className={styles["lcheck-lbl"]}>{Locale.RsClawPanel.Wizard.DetectChecking}</div>
            </div>
          ) : openclawPath ? (
            <div style={{ display: "flex", flexDirection: "column", gap: "8px", marginBottom: "12px" }}>
              <div className={styles["det-row"]}>
                <div className={styles["det-ico"]}>{"\uD83D\uDD04"}</div>
                <div>
                  <div className={styles["det-lbl"]}>{Locale.RsClawPanel.Wizard.DetectFound}</div>
                  <div className={styles["det-sub"]}>{openclawPath}</div>
                </div>
              </div>
              {migrateDone ? (
                <div className={styles["success-box"]}>
                  <div className={styles["success-box-title"]}>{Locale.RsClawPanel.Wizard.MigrateDone}</div>
                </div>
              ) : migrateError ? (
                <div className={styles["note"] + " " + styles["warn"]}>
                  {Locale.RsClawPanel.Wizard.MigrateFailed}: {migrateError}
                </div>
              ) : null}
            </div>
          ) : (
            <div className={styles["note"] + " " + styles["info"]} style={{ marginBottom: "12px" }}>
              <span>i</span>
              <span>{Locale.RsClawPanel.Wizard.DetectNotFound}</span>
            </div>
          )}

          <div className={styles["wiz-nav"]}>
            <span />
            <div style={{ display: "flex", gap: "8px" }}>
              {openclawPath && !migrateDone && (
                <button
                  className={`${styles["btn"]} ${styles["primary"]}`}
                  onClick={handleMigrate}
                  disabled={migrating}
                >
                  {migrating ? Locale.RsClawPanel.Wizard.Migrating : Locale.RsClawPanel.Wizard.MigrateBtn}
                </button>
              )}
              <button
                className={openclawPath && !migrateDone ? styles["btn"] : `${styles["btn"]} ${styles["primary"]}`}
                onClick={() => setStep(1)}
              >
                {openclawPath && !migrateDone
                  ? Locale.RsClawPanel.Wizard.MigrateSkip
                  : `${Locale.RsClawPanel.Wizard.Next} \u2192`}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Step 1: Language */}
      {step === 1 && (
        <div className={styles["wiz-card"]}>
          <div className={styles["wiz-title"]}>{Locale.RsClawPanel.Wizard.Step1Title}</div>
          <div className={styles["wiz-sub"]}>
            {Locale.RsClawPanel.Wizard.Step1Sub}
          </div>
          <div className={styles["provider-grid"]}>
            {LANGUAGES.map((lang) => (
              <div
                key={lang}
                className={`${styles["prov-card"]} ${
                  selectedLang === lang ? styles["sel"] : ""
                }`}
                onClick={() => setSelectedLang(lang)}
              >
                <div className={styles["prov-n"]}>{lang}</div>
              </div>
            ))}
          </div>
          <div className={styles["wiz-nav"]}>
            <button className={styles["btn"]} onClick={() => setStep(0)}>
              &larr; {Locale.RsClawPanel.Wizard.Back}
            </button>
            <button
              className={`${styles["btn"]} ${styles["primary"]}`}
              onClick={() => setStep(2)}
            >
              {Locale.RsClawPanel.Wizard.Next} &rarr;
            </button>
          </div>
        </div>
      )}

      {/* Step 2: Provider */}
      {step === 2 && (
        <div className={styles["wiz-card"]}>
          <div className={styles["wiz-title"]}>{Locale.RsClawPanel.Wizard.Step2Title}</div>
          <div className={styles["wiz-sub"]}>
            {Locale.RsClawPanel.Wizard.Step2Sub}
          </div>
          <div className={styles["provider-grid"]}>
            {PROVIDERS.map((p) => (
              <div
                key={p.id}
                className={`${styles["prov-card"]} ${
                  selectedProviders.includes(p.id) ? styles["sel"] : ""
                }`}
                onClick={() => toggleProvider(p.id)}
              >
                <div className={styles["prov-logo"]}>{p.logo}</div>
                <div>
                  <div className={styles["prov-n"]}>{p.name}</div>
                  <div className={styles["prov-s"]}>{p.desc}</div>
                </div>
              </div>
            ))}
          </div>
          {selectedProviders.filter((p) => p !== "ollama").map((p) => (
            <div
              key={p}
              className={styles["cfg-f"]}
              style={{
                background: "#27272c",
                borderRadius: "9px",
                padding: "12px 13px",
                border: "1px solid rgba(255,255,255,0.06)",
                marginBottom: "8px",
              }}
            >
              <div className={styles["cfg-lbl"]}>
                {p.toUpperCase()} API Key
              </div>
              <input
                type="password"
                className={styles["cfg-input"]}
                value={apiKeys[p] || ""}
                onChange={(e) => setApiKeys({ ...apiKeys, [p]: e.target.value })}
                placeholder="sk-..."
              />
            </div>
          ))}
          <div className={styles["wiz-nav"]}>
            <button className={styles["btn"]} onClick={() => setStep(1)}>
              &larr; {Locale.RsClawPanel.Wizard.Back}
            </button>
            <button
              className={`${styles["btn"]} ${styles["primary"]}`}
              onClick={() => setStep(3)}
            >
              {Locale.RsClawPanel.Wizard.Next} &rarr;
            </button>
          </div>
        </div>
      )}

      {/* Step 3: Channels */}
      {step === 3 && (
        <div className={styles["wiz-card"]}>
          <div className={styles["wiz-title"]}>{Locale.RsClawPanel.Wizard.Step3Title}</div>
          <div className={styles["wiz-sub"]}>
            {Locale.RsClawPanel.Wizard.Step3Sub}
          </div>
          <div className={styles["ch-grid"]}>
            {CHANNEL_OPTIONS.map((ch) => (
              <div
                key={ch.id}
                className={`${styles["ch-pick"]} ${
                  selectedChannels.includes(ch.id) ? styles["sel"] : ""
                }`}
                onClick={() => toggleChannel(ch.id)}
              >
                <div
                  className={styles["ch-pick-ico"]}
                  style={{ color: ch.color }}
                >
                  {ch.icon}
                </div>
                <div className={styles["ch-pick-n"]}>{ch.name}</div>
              </div>
            ))}
          </div>

          {/* WeChat QR scan */}
          {selectedChannels.includes("wechat") && (
            <div className={styles["cfg-f"]} style={{ background: "#27272c", borderRadius: "9px", padding: "12px 13px", border: "1px solid rgba(255,255,255,0.06)", marginBottom: "8px" }}>
              <div className={styles["cfg-lbl"]}>{getLang() === "cn" ? "微信" : "WeChat"}</div>
              {wechatStatus === "connected" ? (
                <div style={{ color: "#3ecf8e", fontSize: "12px" }}>{Locale.RsClawPanel.Wizard.WechatConnected}</div>
              ) : wechatStatus === "expired" ? (
                <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
                  <span style={{ color: "#f06565", fontSize: "12px" }}>{Locale.RsClawPanel.Wizard.WechatExpired}</span>
                  <button className={styles["btn"]} style={{ fontSize: "11px", padding: "3px 10px" }} onClick={async () => {
                    try {
                      const data = await wechatQrStart();
                      setWechatQrUrl(data.qrcode_url);
                      setWechatQrToken(data.qrcode_token);
                      setWechatStatus("scanning");
                    } catch {}
                  }}>{Locale.RsClawPanel.Wizard.WechatScanQR}</button>
                </div>
              ) : wechatStatus === "scanning" ? (
                <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
                  <img src={wechatQrUrl} alt="QR" style={{ width: "180px", height: "180px", borderRadius: "8px", background: "#fff" }} />
                  <span style={{ color: "#f5a623", fontSize: "11px" }}>{Locale.RsClawPanel.Wizard.WechatScanning}</span>
                </div>
              ) : (
                <button className={styles["btn"]} style={{ marginTop: "6px", width: "100%" }} onClick={async () => {
                  try {
                    const data = await wechatQrStart();
                    setWechatQrUrl(data.qrcode_url);
                    setWechatQrToken(data.qrcode_token);
                    setWechatStatus("scanning");
                    // Start polling
                    const poll = setInterval(async () => {
                      try {
                        const result = await wechatQrStatus(data.qrcode_token);
                        if (result.status === "ok") {
                          setWechatStatus("connected");
                          clearInterval(poll);
                        }
                      } catch {
                        setWechatStatus("expired");
                        clearInterval(poll);
                      }
                    }, 3000);
                    // Auto-stop after 2 min
                    setTimeout(() => clearInterval(poll), 120000);
                  } catch {
                    toast.error("QR code", "Failed to get QR code");
                  }
                }}>{Locale.RsClawPanel.Wizard.WechatScanQR}</button>
              )}
            </div>
          )}

          {/* Feishu credentials */}
          {selectedChannels.includes("feishu") && (
            <div className={styles["cfg-f"]} style={{ background: "#27272c", borderRadius: "9px", padding: "12px 13px", border: "1px solid rgba(255,255,255,0.06)", marginBottom: "8px" }}>
              <div className={styles["cfg-lbl"]}>{Locale.RsClawPanel.Wizard.FeishuAppId}</div>
              <input className={styles["cfg-input"]} value={feishuAppId} onChange={(e) => setFeishuAppId(e.target.value)} placeholder="cli_xxx" style={{ marginBottom: "8px" }} />
              <div className={styles["cfg-lbl"]}>{Locale.RsClawPanel.Wizard.FeishuAppSecret}</div>
              <input className={styles["cfg-input"]} type="password" value={feishuAppSecret} onChange={(e) => setFeishuAppSecret(e.target.value)} placeholder="***" />
            </div>
          )}

          <div className={styles["wiz-nav"]}>
            <button className={styles["btn"]} onClick={() => setStep(2)}>
              &larr; {Locale.RsClawPanel.Wizard.Back}
            </button>
            <button
              className={`${styles["btn"]} ${styles["primary"]}`}
              onClick={() => setStep(4)}
            >
              {Locale.RsClawPanel.Wizard.Next} &rarr;
            </button>
          </div>
        </div>
      )}

      {/* Step 4: Launch */}
      {step === 4 && (
        <div className={styles["wiz-card"]}>
          <div className={styles["wiz-title"]}>{Locale.RsClawPanel.Wizard.Step4Title}</div>
          <div className={styles["wiz-sub"]}>
            {Locale.RsClawPanel.Wizard.Step4Sub}
          </div>

          {/* Summary */}
          <div className={styles["note"] + " " + styles["info"]}>
            <span>i</span>
            <span>
              {Locale.RsClawPanel.Wizard.Summary(
                selectedLang,
                selectedProviders.join(", "),
                selectedChannels.join(", "),
              )}
            </span>
          </div>

          {launchChecks.length > 0 && (
            <div className={styles["launch-row"]}>
              {launchChecks.map((check, i) => (
                <div key={i} className={styles["lcheck"]}>
                  <div className={styles["lcheck-ico"]}>
                    {check.status === "ok"
                      ? "\u2705"
                      : check.status === "loading"
                        ? "\u23F3"
                        : "\u2B55"}
                  </div>
                  <div className={styles["lcheck-lbl"]}>{check.label}</div>
                  <div
                    className={`${styles["lcheck-res"]} ${styles[check.status] || ""}`}
                  >
                    {check.status === "ok"
                      ? Locale.RsClawPanel.Wizard.StatusPass
                      : check.status === "loading"
                        ? Locale.RsClawPanel.Wizard.StatusChecking
                        : Locale.RsClawPanel.Wizard.StatusWaiting}
                  </div>
                </div>
              ))}
            </div>
          )}

          {launchDone && (
            <div className={styles["success-box"]}>
              <div className={styles["success-box-title"]}>
                {Locale.RsClawPanel.Wizard.ReadyTitle}
              </div>
              <div className={styles["success-box-body"]}>
                {Locale.RsClawPanel.Wizard.ReadySub}
              </div>
            </div>
          )}

          <div className={styles["wiz-nav"]}>
            <button className={styles["btn"]} onClick={() => setStep(3)}>
              &larr; {Locale.RsClawPanel.Wizard.Back}
            </button>
            {launchDone ? (
              <button
                className={`${styles["btn"]} ${styles["primary"]}`}
                onClick={() => navigate(Path.Home)}
              >
                {Locale.RsClawPanel.Wizard.StartChatting}
              </button>
            ) : (
              <button
                className={`${styles["btn"]} ${styles["primary"]}`}
                onClick={runLaunch}
                disabled={launching}
              >
                {launching ? Locale.RsClawPanel.Wizard.Launching : Locale.RsClawPanel.Wizard.LaunchGateway}
              </button>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

// ══════════════════════════════════════════════════════════
// ── Workspace Editor Page ────────────────────────────────
// ══════════════════════════════════════════════════════════

function WorkspacePage() {
  const [files, setFiles] = useState<string[]>([]);
  const [activeFile, setActiveFile] = useState<string>("");
  const [content, setContent] = useState("");
  const [saving, setSaving] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [agentId, setAgentId] = useState("");
  const [agentList, setAgentList] = useState<{ id: string; name?: string }[]>([]);

  const fetchFiles = useCallback(async (agent?: string) => {
    try {
      const data = await listWorkspaceFiles(agent || undefined);
      setFiles(data.files || []);
      if (data.files?.length > 0 && !activeFile) {
        loadFile(data.files[0], agent);
      }
    } catch {
      setFiles([]);
    }
  }, []);

  const loadFile = async (name: string, agent?: string) => {
    try {
      const data = await readWorkspaceFile(name, agent || undefined);
      setContent(data.content || "");
      setActiveFile(name);
      setDirty(false);
    } catch {
      setContent("");
    }
  };

  const [showNewFile, setShowNewFile] = useState(false);
  const [newFileName, setNewFileName] = useState("");

  const handleSave = async () => {
    if (!activeFile) return;
    setSaving(true);
    try {
      await writeWorkspaceFile(activeFile, content, agentId || undefined);
      setDirty(false);
      toast.success(Locale.RsClawPanel.Workspace.SaveSuccess);
    } catch (e) {
      toast.fromError(Locale.RsClawPanel.Workspace.SaveFailed, e);
    } finally {
      setSaving(false);
    }
  };

  const handleCreateFile = async () => {
    let name = newFileName.trim();
    if (!name) return;
    if (!name.endsWith(".md")) name += ".md";
    // Create with empty content
    try {
      await writeWorkspaceFile(name, `# ${name}\n\n`, agentId || undefined);
      setShowNewFile(false);
      setNewFileName("");
      await fetchFiles(agentId);
      loadFile(name, agentId);
    } catch (e) {
      toast.fromError(Locale.RsClawPanel.Workspace.SaveFailed, e);
    }
  };

  useEffect(() => {
    getAgents()
      .then((data) => {
        const list = Array.isArray(data) ? data : data.agents || [];
        setAgentList(list);
      })
      .catch(() => {});
    fetchFiles();
  }, []);

  const switchAgent = (id: string) => {
    setAgentId(id);
    setActiveFile("");
    setContent("");
    setDirty(false);
    fetchFiles(id);
  };

  return (
    <div>
      <div className={styles["page-header"]}>
        <div>
          <div className={styles["page-title"]}>{Locale.RsClawPanel.Workspace.PageTitle}</div>
          <div className={styles["page-sub"]}>{Locale.RsClawPanel.Workspace.PageSub}</div>
        </div>
        <div style={{ display: "flex", gap: "8px", alignItems: "center" }}>
          {agentList.length > 0 && (
            <select
              className={styles["cfg-select"]}
              value={agentId}
              onChange={(e) => switchAgent(e.target.value)}
              style={{ width: "auto", minWidth: "120px" }}
            >
              <option value="">{Locale.RsClawPanel.Workspace.DefaultAgent}</option>
              {agentList.map((a) => (
                <option key={a.id} value={a.id}>{a.name || a.id}</option>
              ))}
            </select>
          )}
          <button
            className={`${styles["btn"]} ${styles["primary"]}`}
            onClick={handleSave}
            disabled={saving || !dirty}
          >
            {saving ? Locale.RsClawPanel.Config.Saving : Locale.RsClawPanel.Workspace.Save}
          </button>
        </div>
      </div>

      <div className={styles["ws-layout"]}>
        <div className={styles["ws-file-list"]}>
          {files.map((f) => (
            <button
              key={f}
              className={`${styles["ws-file-item"]} ${f === activeFile ? styles["active"] : ""}`}
              onClick={() => loadFile(f, agentId)}
            >
              {f}
            </button>
          ))}
          {showNewFile ? (
            <div className={styles["ws-new-file"]}>
              <input
                autoFocus
                className={styles["cfg-input"]}
                value={newFileName}
                placeholder={Locale.RsClawPanel.Workspace.NewFilePlaceholder}
                onChange={(e) => setNewFileName(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter") handleCreateFile(); }}
                style={{ fontSize: "11px", padding: "5px 8px" }}
              />
              <div style={{ display: "flex", gap: "4px", marginTop: "4px" }}>
                <button className={styles["btn"]} onClick={() => { setShowNewFile(false); setNewFileName(""); }} style={{ fontSize: "10px", padding: "3px 8px" }}>
                  {Locale.RsClawPanel.Workspace.Cancel}
                </button>
                <button className={`${styles["btn"]} ${styles["primary"]}`} onClick={handleCreateFile} style={{ fontSize: "10px", padding: "3px 8px" }}>
                  {Locale.RsClawPanel.Workspace.Create}
                </button>
              </div>
            </div>
          ) : (
            <button
              className={styles["ws-file-item"]}
              onClick={() => setShowNewFile(true)}
              style={{ color: "#f97316" }}
            >
              + {Locale.RsClawPanel.Workspace.NewFile}
            </button>
          )}
        </div>
        <div className={styles["ws-editor"]}>
          <textarea
            className={styles["ws-textarea"]}
            value={content}
            onChange={(e) => { setContent(e.target.value); setDirty(true); }}
            placeholder={Locale.RsClawPanel.Workspace.EditorPlaceholder}
            spellCheck={false}
          />
        </div>
      </div>
    </div>
  );
}

// ══════════════════════════════════════════════════════════
// ── Main RsClaw Panel ────────────────────────────────────
// ══════════════════════════════════════════════════════════
// ══════════════════════════════════════════════════════════
// ── Doctor Page ──────────────────────────────────────────
// ══════════════════════════════════════════════════════════

interface DoctorCheck {
  status: string;
  message: string;
}

function DoctorPage() {
  const [checks, setChecks] = useState<DoctorCheck[]>([]);
  const [checking, setChecking] = useState(false);
  const [fixing, setFixing] = useState(false);
  const [hasRun, setHasRun] = useState(false);

  // Parse CLI output lines into DoctorCheck items
  const parseOutput = (output: string): DoctorCheck[] => {
    const lines = output.split("\n");
    const results: DoctorCheck[] = [];
    for (const line of lines) {
      const trimmed = line.trim();
      if (trimmed.includes("[ok]")) {
        results.push({ status: "ok", message: trimmed.replace(/\[ok\]/g, "").trim() });
      } else if (trimmed.includes("[warn]")) {
        results.push({ status: "warn", message: trimmed.replace(/\[warn\]/g, "").trim() });
      } else if (trimmed.includes("[fixed]")) {
        results.push({ status: "fixed", message: trimmed.replace(/\[fixed\]/g, "").trim() });
      } else if (trimmed.includes("[fix-failed]")) {
        results.push({ status: "error", message: trimmed.replace(/\[fix-failed\]/g, "").trim() });
      } else if (trimmed.includes("checks passed")) {
        results.push({ status: "ok", message: trimmed });
      }
    }
    return results;
  };

  const runCliDoctor = async (fix: boolean) => {
    const invoke = (window as any).__TAURI__?.invoke;
    if (!invoke) {
      toast.error("Tauri not available");
      return;
    }
    try {
      const args = fix ? ["doctor", "--fix", "--yes"] : ["doctor"];
      const output: string = await invoke("run_rsclaw_cli", { args });
      setChecks(parseOutput(output));
      setHasRun(true);
    } catch (e: any) {
      // CLI may write to both stdout and stderr; try to parse error output too
      const errStr = String(e?.message || e || "");
      const parsed = parseOutput(errStr);
      if (parsed.length > 0) {
        setChecks(parsed);
        setHasRun(true);
      } else {
        toast.fromError("Doctor", e);
      }
    }
  };

  const handleCheck = async () => {
    setChecking(true);
    try { await runCliDoctor(false); } finally { setChecking(false); }
  };

  const handleFix = async () => {
    setFixing(true);
    try { await runCliDoctor(true); } finally { setFixing(false); }
  };

  const statusIcon = (s: string) => {
    switch (s) {
      case "ok": return "\u2705";
      case "warn": return "\u26A0\uFE0F";
      case "error": return "\u274C";
      case "fixed": return "\uD83D\uDD27";
      default: return "\u2B55";
    }
  };

  return (
    <div>
      <div className={styles["page-header"]}>
        <div>
          <div className={styles["page-title"]}>{Locale.RsClawPanel.Doctor.PageTitle}</div>
          <div className={styles["page-sub"]}>{Locale.RsClawPanel.Doctor.PageSub}</div>
        </div>
        <div style={{ display: "flex", gap: "8px" }}>
          <button
            className={styles["btn"]}
            onClick={handleCheck}
            disabled={checking || fixing}
          >
            {checking ? Locale.RsClawPanel.Doctor.Running : Locale.RsClawPanel.Doctor.RunCheck}
          </button>
          <button
            className={`${styles["btn"]} ${styles["primary"]}`}
            onClick={handleFix}
            disabled={checking || fixing}
          >
            {fixing ? Locale.RsClawPanel.Doctor.Fixing : Locale.RsClawPanel.Doctor.RunFix}
          </button>
        </div>
      </div>

      {!hasRun && (
        <div className={styles["empty-state"]}>
          {Locale.RsClawPanel.Doctor.NotRun}
        </div>
      )}

      {hasRun && checks.length === 0 && (
        <div className={styles["success-box"]}>
          <div className={styles["success-box-title"]}>{Locale.RsClawPanel.Doctor.NoIssues}</div>
        </div>
      )}

      {checks.length > 0 && (
        <div className={styles["launch-row"]}>
          {checks.map((check, i) => (
            <div key={i} className={styles["lcheck"]}>
              <div className={styles["lcheck-ico"]}>{statusIcon(check.status)}</div>
              <div className={styles["lcheck-lbl"]}>{check.message}</div>
              <div className={`${styles["lcheck-res"]} ${
                check.status === "ok" || check.status === "fixed" ? styles["ok"] :
                check.status === "warn" ? styles["loading"] : ""
              }`}>
                {check.status.toUpperCase()}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// ══════════════════════════════════════════════════════════
// ── Tauri Config Page (simple raw editor, no structured form) ────
// ══════════════════════════════════════════════════════════

function TauriConfigPageInner() {
  const zh = getLang() === "cn";
  const [raw, setRaw] = useState("");
  const [config, setConfig] = useState<any>({});
  const [cfgPath, setCfgPath] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [parseError, setParseError] = useState("");
  const [activeTab, setActiveTab] = useState<"gateway"|"models"|"channels"|"tools"|"raw">("gateway");

  // Provider state: open cards, test status, fetched model lists, selected model
  const [openProvs, setOpenProvs] = useState<Set<string>>(new Set());
  const [provTest, setProvTest] = useState<Record<string, "idle"|"testing"|"ok"|"err">>({});
  const [provErr, setProvErr] = useState<Record<string, string>>({});
  const [provModels, setProvModels] = useState<Record<string, { id: string; tag: string }[]>>({});
  const [provSelModel, setProvSelModel] = useState<Record<string, string>>({});

  // Channel state: open cards, login tab per channel, open accounts, account tab
  const [openChs, setOpenChs] = useState<Set<string>>(new Set());
  const [chLoginTab, setChLoginTab] = useState<Record<string, "qr"|"cred">>({});
  const [openAccts, setOpenAccts] = useState<Record<string, boolean>>({});
  const [acctTab, setAcctTab] = useState<Record<string, "qr"|"cred">>({});

  // QR state
  const [qrData, setQrData] = useState<Record<string, string>>({});
  const [qrStatus, setQrStatus] = useState<Record<string, string>>({});

  // ── Config path constants ──
  const V = {
    bg0: "#080809", bg1: "#0f1013", bg2: "#141618", bg3: "#1a1c22", bg4: "#1f2126", bg5: "#252830",
    bd: "rgba(255,255,255,.055)", bd2: "rgba(255,255,255,.09)", bd3: "rgba(255,255,255,.14)",
    t0: "#eceaf4", t1: "#9896a4", t2: "#4a4858", t3: "#2e2c3a",
    or: "#f97316", or2: "#fb923c", olo: "rgba(249,115,22,.09)", obrd: "rgba(249,115,22,.2)",
    green: "#2dd4a0", glo: "rgba(45,212,160,.07)", gbrd: "rgba(45,212,160,.18)",
    red: "#d95f5f", rlo: "rgba(217,95,95,.08)", rbrd: "rgba(217,95,95,.18)",
    mono: "'JetBrains Mono', monospace", sans: "'Geist', sans-serif",
  };

  const fInput: React.CSSProperties = { background: V.bg4, border: `1px solid ${V.bd2}`, borderRadius: 7, padding: "7px 10px", color: V.t0, fontFamily: V.mono, fontSize: 11.5, outline: "none", minWidth: 160 };
  const fSelect: React.CSSProperties = { background: V.bg4, border: `1px solid ${V.bd2}`, borderRadius: 7, padding: "7px 10px", color: V.t0, fontFamily: V.sans, fontSize: 12, outline: "none", minWidth: 200, cursor: "pointer" };
  const fieldRow: React.CSSProperties = { display: "flex", alignItems: "center", gap: 14, padding: "11px 15px", borderBottom: "1px solid rgba(255,255,255,.03)" };
  const fcard: React.CSSProperties = { background: V.bg2, border: `1px solid ${V.bd}`, borderRadius: 11, overflow: "hidden", marginBottom: 10 };

  // ── Load config on mount ──
  useEffect(() => {
    (async () => {
      try {
        const invoke = (window as any).__TAURI__?.invoke;
        if (invoke) {
          const cp: string = await invoke("get_config_path");
          setCfgPath(cp ? cp + "/rsclaw.json5" : "~/.rsclaw/rsclaw.json5");
          const content: string = await invoke("read_config_file");
          setRaw(content || "{}");
          try { setConfig(JSON.parse(content || "{}")); } catch { setConfig({}); }
        }
      } catch {}
      setLoading(false);
    })();
  }, []);

  // ── Auto-detect configured providers on load ──
  useEffect(() => {
    if (!config?.models?.providers) return;
    const provs = config.models.providers;
    const testState: Record<string, "idle"|"testing"|"ok"|"err"> = {};
    const selModels: Record<string, string> = {};
    const modelLists: Record<string, { id: string; tag: string }[]> = {};
    for (const [provId, provConf] of Object.entries(provs) as [string, any][]) {
      const apiKey = provConf?.apiKey || "";
      const baseUrl = provConf?.baseUrl || "";
      const provDef = ALL_PROVIDERS[provId];
      const hasKey = !!apiKey || provDef?.isUrl;
      if (!hasKey) continue;
      // Check if provider has a selected default model in config
      let selModel = "";
      // Check agents.defaults.model.primary first
      const primary = config?.agents?.defaults?.model?.primary || "";
      if (primary.startsWith(provId + "/")) {
        selModel = primary.split("/").slice(1).join("/");
      }
      // Also check agents.defaults.models (alias table)
      if (!selModel) {
        const defaultModels = config?.agents?.defaults?.models || {};
        for (const [, val] of Object.entries(defaultModels) as [string, any][]) {
          const modelStr = typeof val === "string" ? val : val?.model || "";
          if (modelStr.startsWith(provId + "/")) { selModel = modelStr.split("/").slice(1).join("/"); break; }
        }
      }
      // If provider has key configured, mark as connected
      testState[provId] = "ok";
      if (selModel) selModels[provId] = selModel;
      // Add fallback models from MODELS constant
      if (MODELS[provId]) {
        modelLists[provId] = MODELS[provId].map((m) => ({ id: m.id, tag: zh ? m.tag : m.tagEn }));
      }
    }
    if (Object.keys(testState).length > 0) {
      setProvTest((prev) => ({ ...prev, ...testState }));
      setProvSelModel((prev) => ({ ...prev, ...selModels }));
      setProvModels((prev) => ({ ...prev, ...modelLists }));
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [config?.models?.providers ? JSON.stringify(Object.keys(config.models.providers)) : ""]);

  // ── Helpers ──
  const updateConfig = (dotPath: string, value: any) => {
    const parts = dotPath.split(".");
    const newConfig = JSON.parse(JSON.stringify(config));
    let cur = newConfig;
    for (let i = 0; i < parts.length - 1; i++) {
      if (!cur[parts[i]] || typeof cur[parts[i]] !== "object") cur[parts[i]] = {};
      cur = cur[parts[i]];
    }
    cur[parts[parts.length - 1]] = value;
    setConfig(newConfig);
    setRaw(JSON.stringify(newConfig, null, 2));
    setDirty(true);
  };

  const getVal = (dotPath: string, def: any = "") => {
    const parts = dotPath.split(".");
    let cur = config;
    for (const p of parts) { if (!cur || typeof cur !== "object") return def; cur = cur[p]; }
    return cur ?? def;
  };

  const deleteConfig = (dotPath: string) => {
    const parts = dotPath.split(".");
    const newConfig = JSON.parse(JSON.stringify(config));
    let cur = newConfig;
    for (let i = 0; i < parts.length - 1; i++) {
      if (!cur[parts[i]] || typeof cur[parts[i]] !== "object") return;
      cur = cur[parts[i]];
    }
    delete cur[parts[parts.length - 1]];
    setConfig(newConfig);
    setRaw(JSON.stringify(newConfig, null, 2));
    setDirty(true);
  };

  const handleSave = async () => {
    try { JSON.parse(raw); } catch { toast.error(zh ? "JSON 格式错误" : "Invalid JSON"); return; }
    setSaving(true);
    try {
      const invoke = (window as any).__TAURI__?.invoke;
      if (invoke) {
        await invoke("write_config", { content: raw });
        try { await reloadConfig(); } catch {}
        toast.success(zh ? "配置已保存" : "Config saved");
        setDirty(false);
      }
    } catch (e: any) { toast.fromError(zh ? "保存失败" : "Save failed", e); }
    setSaving(false);
  };

  // All hooks must be before any early return (React rules of hooks)
  const qrPollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  if (loading) return <div style={{ padding: 40, textAlign: "center", color: V.t1 }}>...</div>;

  // ── Ordered providers / channels ──
  const provOrder = zh ? PROV_ORDER_ZH : PROV_ORDER_EN;
  const provList = provOrder.map((id) => ALL_PROVIDERS[id]).filter(Boolean);
  const chOrder = zh ? CH_ORDER_ZH : CH_ORDER_EN;
  const chList = chOrder.map((id) => ALL_CHANNELS[id]).filter(Boolean);

  const CRED_FIELDS: Record<string, { key: string; label: string; type: string; ph: string }[]> = {
    wechat:   [{ key: "botId", label: "Bot ID", type: "text", ph: "xxx@im.bot" }, { key: "botToken", label: "Bot Token", type: "password", ph: "${WECHAT_BOT_TOKEN}" }],
    feishu:   [{ key: "appId", label: "App ID", type: "text", ph: "cli_xxx" }, { key: "appSecret", label: "App Secret", type: "password", ph: "${FEISHU_APP_SECRET}" }],
    wecom:    [{ key: "botId", label: "Bot ID", type: "text", ph: "" }, { key: "secret", label: "Secret", type: "password", ph: "${WECOM_SECRET}" }],
    dingtalk: [{ key: "appKey", label: "App Key", type: "text", ph: "" }, { key: "appSecret", label: "App Secret", type: "password", ph: "${DINGTALK_APP_SECRET}" }],
    telegram: [{ key: "botToken", label: "Bot Token", type: "password", ph: "${TELEGRAM_BOT_TOKEN}" }],
    discord:  [{ key: "token", label: "Bot Token", type: "password", ph: "${DISCORD_BOT_TOKEN}" }],
    slack:    [{ key: "botToken", label: "Bot Token", type: "password", ph: "${SLACK_BOT_TOKEN}" }, { key: "appToken", label: "App Token", type: "password", ph: "${SLACK_APP_TOKEN}" }],
    whatsapp: [{ key: "phoneNumberId", label: "Phone Number ID", type: "text", ph: "" }, { key: "accessToken", label: "Access Token", type: "password", ph: "${WHATSAPP_TOKEN}" }],
    qq:       [{ key: "appId", label: "App ID", type: "text", ph: "" }, { key: "appSecret", label: "App Secret", type: "password", ph: "${QQ_APP_SECRET}" }],
    line:     [{ key: "channelSecret", label: "Channel Secret", type: "password", ph: "${LINE_CHANNEL_SECRET}" }, { key: "channelAccessToken", label: "Access Token", type: "password", ph: "${LINE_ACCESS_TOKEN}" }],
    zalo:     [{ key: "appId", label: "App ID", type: "text", ph: "" }, { key: "accessToken", label: "Access Token", type: "password", ph: "${ZALO_ACCESS_TOKEN}" }],
    matrix:   [{ key: "homeserver", label: "Homeserver", type: "text", ph: "https://matrix.org" }, { key: "userId", label: "User ID", type: "text", ph: "@bot:matrix.org" }, { key: "accessToken", label: "Access Token", type: "password", ph: "${MATRIX_ACCESS_TOKEN}" }],
    signal:   [{ key: "phoneNumber", label: "Phone Number", type: "text", ph: "+1234567890" }],
  };

  const LANGUAGES = [
    { value: "Chinese", label: "Chinese (\u4E2D\u6587)" },
    { value: "English", label: "English" },
    { value: "Japanese", label: "Japanese (\u65E5\u672C\u8A9E)" },
    { value: "Korean", label: "Korean (\uD55C\uAD6D\uC5B4)" },
    { value: "Thai", label: "Thai (\u0E44\u0E17\u0E22)" },
    { value: "Vietnamese", label: "Vietnamese (Ti\u1EBFng Vi\u1EC7t)" },
    { value: "French", label: "French (Fran\u00E7ais)" },
    { value: "German", label: "German (Deutsch)" },
    { value: "Spanish", label: "Spanish (Espa\u00F1ol)" },
    { value: "Russian", label: "Russian (\u0420\u0443\u0441\u0441\u043A\u0438\u0439)" },
  ];

  // ── Section heading ──
  const secHead = (title: string) => (
    <div style={{ display: "flex", alignItems: "center", gap: 10, marginBottom: 12 }}>
      <div style={{ fontSize: 11, fontWeight: 600, color: V.t1, letterSpacing: 0.4, textTransform: "uppercase" as const }}>{title}</div>
      <div style={{ flex: 1, height: 1, background: V.bd }} />
    </div>
  );

  // ── Toggle helpers ──
  const toggleProv = (id: string) => {
    setOpenProvs((prev) => {
      const s = new Set(prev);
      if (s.has(id)) { s.delete(id); } else {
        s.add(id);
        // Auto-test when opening a provider card that has a key but hasn't been tested
        const apiKey = getVal(`models.providers.${id}.apiKey`, "");
        const baseUrl = getVal(`models.providers.${id}.baseUrl`, "");
        const hasKey = !!apiKey || ALL_PROVIDERS[id]?.isUrl;
        const notTested = !provTest[id] || provTest[id] === "idle";
        if (hasKey && notTested) {
          setTimeout(() => handleTestProvider(id), 100);
        }
      }
      return s;
    });
  };
  const toggleCh = (id: string) => {
    setOpenChs((prev) => { const s = new Set(prev); if (s.has(id)) s.delete(id); else s.add(id); return s; });
  };

  // ── Provider test ──
  const handleTestProvider = async (provId: string) => {
    const apiKey = getVal(`models.providers.${provId}.apiKey`, "");
    const baseUrl = getVal(`models.providers.${provId}.baseUrl`, "");
    if (!apiKey && !ALL_PROVIDERS[provId]?.isUrl) {
      toast.error(zh ? "请先填写 API Key" : "Enter API Key first");
      return;
    }
    setProvTest((prev) => ({ ...prev, [provId]: "testing" }));
    setProvErr((prev) => ({ ...prev, [provId]: "" }));
    try {
      // Test directly via Tauri (no gateway needed) or fallback to gateway API
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      let res: any;
      if (tauriInvoke) {
        res = await tauriInvoke("test_provider", { provider: provId, apiKey, baseUrl: baseUrl || null });
      } else {
        res = await testProviderKey(provId, apiKey, baseUrl || undefined);
      }
      if (res.ok || res.success) {
        // Must have models to be considered connected
        const apiModels: string[] = (res.models || []).map((m: any) => typeof m === "string" ? m : m.id).filter(Boolean);
        if (apiModels.length > 0) {
          setProvTest((prev) => ({ ...prev, [provId]: "ok" }));
          setProvModels((prev) => ({ ...prev, [provId]: apiModels.slice(0, 30).map((id) => {
            const fallback = (MODELS[provId] || []).find((fm) => fm.id === id);
            return { id, tag: fallback ? (zh ? fallback.tag : fallback.tagEn) : "" };
          }) }));
          toast.success(zh ? `${provId} \u8FDE\u63A5\u6210\u529F (${apiModels.length} \u4E2A\u6A21\u578B)` : `${provId} connected (${apiModels.length} models)`);
        } else {
          // API key works but no models returned
          setProvTest((prev) => ({ ...prev, [provId]: "err" }));
          setProvErr((prev) => ({ ...prev, [provId]: zh ? "API Key \u6709\u6548\u4F46\u672A\u83B7\u53D6\u5230\u6A21\u578B\u5217\u8868" : "API Key valid but no models returned" }));
        }
      } else {
        setProvTest((prev) => ({ ...prev, [provId]: "err" }));
        setProvErr((prev) => ({ ...prev, [provId]: res.error || res.message || "Connection failed" }));
      }
    } catch (e: any) {
      setProvTest((prev) => ({ ...prev, [provId]: "err" }));
      setProvErr((prev) => ({ ...prev, [provId]: e?.message || "Connection failed" }));
    }
  };

  const handleSelectModel = (provId: string, modelId: string) => {
    setProvSelModel((prev) => ({ ...prev, [provId]: modelId }));
    updateConfig("agents.defaults.model.primary", `${provId}/${modelId}`);
    toast.success(zh ? `默认模型: ${provId}/${modelId}` : `Default model: ${provId}/${modelId}`);
  };

  // ── Chevron SVG ──
  const Chevron = ({ open }: { open: boolean }) => (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke={V.t3} strokeWidth="2" strokeLinecap="round" style={{ transition: "transform .2s", transform: open ? "rotate(180deg)" : "none", flexShrink: 0 }}><polyline points="6 9 12 15 18 9" /></svg>
  );

  // ── Toggle switch ──
  const Toggle = ({ on, onClick }: { on: boolean; onClick: () => void }) => (
    <div onClick={onClick} style={{ width: 32, height: 18, borderRadius: 9, background: on ? V.or : V.bg5, position: "relative", cursor: "pointer", flexShrink: 0, transition: "background .18s" }}>
      <div style={{ position: "absolute", top: 2, left: on ? 16 : 2, width: 14, height: 14, borderRadius: "50%", background: "#fff", transition: "left .18s" }} />
    </div>
  );

  // ── Spinner ──
  const Spinner = ({ color }: { color?: string }) => (
    <span style={{ display: "inline-block", width: 12, height: 12, border: `1.5px solid ${color === "green" ? "rgba(45,212,160,.2)" : "rgba(249,115,22,.2)"}`, borderTopColor: color === "green" ? V.green : V.or, borderRadius: "50%", animation: "spin .7s linear infinite", verticalAlign: "middle", flexShrink: 0 }} />
  );

  // ── Tabs config ──
  const tabs: { key: typeof activeTab; label: string }[] = [
    { key: "gateway", label: zh ? "\u7F51\u5173" : "Gateway" },
    { key: "models", label: zh ? "\u6A21\u578B\u63D0\u4F9B\u5546" : "Models" },
    { key: "channels", label: zh ? "\u6D88\u606F\u901A\u9053" : "Channels" },
    { key: "tools", label: zh ? "\u5DE5\u5177 & \u529F\u80FD" : "Tools" },
    { key: "raw", label: "JSON5" },
  ];

  // Bind address state
  const bindVal = getVal("gateway.bind", "loopback");
  const isCustomBind = bindVal !== "loopback" && bindVal !== "all";

  const handleQrStart = async (chId: string) => {
    setQrStatus((prev) => ({ ...prev, [chId]: zh ? "\u83B7\u53D6\u4E2D..." : "Loading..." }));
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) {
        // Use sidecar to start channel login
        await tauriInvoke("channel_login_start", { channel: chId });
        // Poll for QR image
        if (qrPollRef.current) clearInterval(qrPollRef.current);
        let attempts = 0;
        qrPollRef.current = setInterval(async () => {
          attempts++;
          try {
            // Check login status
            const status: string = await tauriInvoke("channel_login_status");
            if (status === "done") {
              if (qrPollRef.current) clearInterval(qrPollRef.current);
              setQrStatus((prev) => ({ ...prev, [chId]: zh ? "\u2713 \u767B\u5F55\u6210\u529F" : "\u2713 Login success" }));
              // Reload config to pick up token written by channel login command
              try {
                const content: string = await tauriInvoke("read_config_file");
                const updated = JSON.parse(content || "{}");
                setConfig(updated);
                setRaw(JSON.stringify(updated, null, 2));
              } catch {}
              return;
            }
            // Check for QR image
            const dataUri: string | null = await tauriInvoke("channel_login_qr");
            if (dataUri) {
              setQrData((prev) => ({ ...prev, [chId]: dataUri }));
              setQrStatus((prev) => ({ ...prev, [chId]: zh ? "\u7B49\u5F85\u626B\u7801..." : "Waiting for scan..." }));
            }
          } catch {}
          if (attempts > 60) { if (qrPollRef.current) clearInterval(qrPollRef.current); }
        }, 2000);
      } else {
        // Fallback: gateway API
        const res = await wechatQrStart();
        if (res.qrcode_url || res.qr_url || res.url) {
          setQrData((prev) => ({ ...prev, [chId]: res.qrcode_url || res.qr_url || res.url }));
          setQrStatus((prev) => ({ ...prev, [chId]: zh ? "\u7B49\u5F85\u626B\u7801..." : "Waiting..." }));
        }
      }
    } catch {
      setQrStatus((prev) => ({ ...prev, [chId]: zh ? "\u83B7\u53D6\u5931\u8D25" : "Failed to get QR" }));
    }
  };

  // ── Multi-account channel helpers ──
  const getChannelAccounts = (chId: string): { id: string; data: any }[] => {
    const chConf = getVal(`channels.${chId}`, {});
    if (chConf.accounts && typeof chConf.accounts === "object") {
      return Object.keys(chConf.accounts).map((k) => ({ id: k, data: chConf.accounts[k] || {} }));
    }
    // Legacy flat config: check if any credential field is set
    const fields = CRED_FIELDS[chId] || [];
    const hasFlat = fields.some((f) => chConf[f.key]);
    if (hasFlat) return [{ id: "default", data: chConf }];
    return [];
  };

  const addAccount = (chId: string) => {
    // Single atomic config update to avoid stale state overwrites
    const newConfig = JSON.parse(JSON.stringify(config));
    // Ensure channels.{chId} exists
    if (!newConfig.channels) newConfig.channels = {};
    if (!newConfig.channels[chId]) newConfig.channels[chId] = {};
    const chConf = newConfig.channels[chId];

    // Migrate legacy flat fields into accounts.default if needed
    if (!chConf.accounts || typeof chConf.accounts !== "object") {
      const fields = CRED_FIELDS[chId] || [];
      const hasFlat = fields.some((f) => chConf[f.key]);
      if (hasFlat) {
        const acctData: any = {};
        fields.forEach((f) => { if (chConf[f.key]) { acctData[f.key] = chConf[f.key]; delete chConf[f.key]; } });
        if (chConf.dmPolicy) { acctData.dmPolicy = chConf.dmPolicy; delete chConf.dmPolicy; }
        if (chConf.label) { acctData.label = chConf.label; delete chConf.label; }
        chConf.accounts = { default: acctData };
      } else {
        chConf.accounts = {};
      }
    }

    // Add the new account
    const newId = chId + "-" + Date.now().toString(36);
    chConf.accounts[newId] = { label: "" };

    setConfig(newConfig);
    setRaw(JSON.stringify(newConfig, null, 2));
    setDirty(true);
    setOpenChs((prev) => new Set(prev).add(chId));
    setOpenAccts((prev) => ({ ...prev, [`${chId}-${newId}`]: true }));
  };

  const removeAccount = (chId: string, acctId: string) => {
    deleteConfig(`channels.${chId}.accounts.${acctId}`);
    setOpenAccts((prev) => { const n = { ...prev }; delete n[`${chId}-${acctId}`]; return n; });
  };

  const toggleAcct = (chId: string, acctId: string) => {
    const key = `${chId}-${acctId}`;
    setOpenAccts((prev) => ({ ...prev, [key]: !prev[key] }));
  };

  const DM_POLICIES = ["open", "pairing", "allowlist", "disabled"];

  return (
    <div style={{ display: "flex", flexDirection: "column", height: "100%", overflow: "hidden" }}>
      {/* Global keyframes - moved to useEffect to avoid SSR issues */}

      {/* Tab bar */}
      <div style={{ display: "flex", alignItems: "center", padding: "0 24px", borderBottom: `1px solid ${V.bd}`, background: V.bg1, flexShrink: 0 }}>
        {tabs.map((tab) => (
          <button key={tab.key} onClick={() => setActiveTab(tab.key)}
            style={{ padding: "13px 16px", fontSize: 12, fontWeight: 500, color: activeTab === tab.key ? V.or : V.t2, cursor: "pointer", borderBottom: `2px solid ${activeTab === tab.key ? V.or : "transparent"}`, marginBottom: -1, background: "none", border: "none", borderBottomWidth: 2, borderBottomStyle: "solid", borderBottomColor: activeTab === tab.key ? V.or : "transparent", display: "flex", alignItems: "center", gap: 6, fontFamily: "inherit", whiteSpace: "nowrap", transition: "all .13s" }}>
            {tab.label}
          </button>
        ))}
        <div style={{ marginLeft: "auto", display: "flex", alignItems: "center", gap: 8, padding: "6px 0" }}>
          <button onClick={handleSave} disabled={saving || !dirty}
            style={{ fontSize: 11, fontWeight: 600, padding: "6px 16px", borderRadius: 7, border: "none", background: dirty ? V.or : V.bg4, color: dirty ? "#fff" : V.t3, cursor: dirty ? "pointer" : "default", boxShadow: dirty ? "0 2px 8px rgba(249,115,22,.25)" : "none", transition: "all .13s" }}>
            {saving ? "..." : (zh ? "\u4FDD\u5B58" : "Save")}
          </button>
        </div>
      </div>

      {/* Content pane */}
      <div style={{ flex: 1, overflowY: "auto", padding: 24 }}>

        {/* ══ GATEWAY TAB ══ */}
        {activeTab === "gateway" && (<div style={{ animation: "fi .15s ease" }}>
          {/* Server section */}
          {secHead(zh ? "\u670D\u52A1\u5668" : "SERVER")}
          <div style={fcard}>
            {/* Bind address */}
            <div style={fieldRow}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u7ED1\u5B9A\u5730\u5740" : "Bind Address"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>gateway.bind</div>
                <div style={{ fontSize: 10, color: V.t3, marginTop: 2, lineHeight: 1.5 }}>{zh ? "\u63A7\u5236\u54EA\u4E9B\u7F51\u7EDC\u63A5\u53E3\u53EF\u4EE5\u8BBF\u95EE\u7F51\u5173" : "Controls which network interfaces can access the gateway"}</div>
              </div>
              <select style={{ ...fSelect, minWidth: 260 }}
                value={isCustomBind ? "custom" : bindVal}
                onChange={(e) => {
                  const v = e.target.value;
                  if (v === "loopback") updateConfig("gateway.bind", "loopback");
                  else if (v === "all") updateConfig("gateway.bind", "all");
                  else updateConfig("gateway.bind", "");
                }}>
                <option value="loopback">loopback {zh ? "\u2014 \u4EC5\u672C\u673A 127.0.0.1\uFF08\u63A8\u8350\uFF09" : "-- localhost only (recommended)"}</option>
                <option value="all">all {zh ? "\u2014 \u6240\u6709\u7F51\u7EDC\u63A5\u53E3 0.0.0.0" : "-- all interfaces, 0.0.0.0"}</option>
                <option value="custom">custom {zh ? "\u2014 \u81EA\u5B9A\u4E49 IP \u5730\u5740" : "-- custom IP address"}</option>
              </select>
            </div>
            {/* Bind warning for "all" */}
            {bindVal === "all" && (
              <div style={fieldRow}>
                <div style={{ display: "flex", alignItems: "flex-start", gap: 8, padding: "9px 12px", borderRadius: 7, background: V.olo, border: `1px solid ${V.obrd}`, fontSize: 11, color: "#b07238", lineHeight: 1.55, width: "100%" }}>
                  {zh ? "\u26A0 \u5C40\u57DF\u7F51\u5185\u6240\u6709\u8BBE\u5907\u5C06\u53EF\u8BBF\u95EE\u7F51\u5173\uFF0C\u8BF7\u786E\u4FDD\u7F51\u7EDC\u73AF\u5883\u5B89\u5168\u3002" : "\u26A0 All devices on the network will be able to access the gateway."}
                </div>
              </div>
            )}
            {/* Custom IP row */}
            {isCustomBind && (
              <div style={fieldRow}>
                <div style={{ flex: 1 }}><div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u81EA\u5B9A\u4E49 IP" : "Custom IP"}</div></div>
                <input style={{ ...fInput, minWidth: 200 }} type="text" placeholder="192.168.1.100" value={bindVal} onChange={(e) => updateConfig("gateway.bind", e.target.value)} />
              </div>
            )}
            {/* Port */}
            <div style={fieldRow}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u7AEF\u53E3" : "Port"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>gateway.port</div>
              </div>
              <input style={{ ...fInput, minWidth: 100 }} type="number" value={getVal("gateway.port", 18888)} onChange={(e) => updateConfig("gateway.port", parseInt(e.target.value) || 18888)} />
            </div>
            {/* Language */}
            <div style={fieldRow}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u7F51\u5173\u8BED\u8A00" : "Gateway Language"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>gateway.language</div>
              </div>
              <select style={{ ...fSelect, minWidth: 180 }} value={getVal("gateway.language", "Chinese")} onChange={(e) => updateConfig("gateway.language", e.target.value)}>
                {LANGUAGES.map((l) => <option key={l.value} value={l.value}>{l.label}</option>)}
              </select>
            </div>
            {/* Processing timeout */}
            <div style={{ ...fieldRow, borderBottom: "none" }}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u5904\u7406\u4E2D\u63D0\u793A\u5EF6\u8FDF" : "Processing Timeout"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>gateway.processingTimeout ({zh ? "\u79D2\uFF0C0=\u7981\u7528" : "sec, 0=disabled"})</div>
              </div>
              <input style={{ ...fInput, minWidth: 100 }} type="number" value={getVal("gateway.processingTimeout", 60)} onChange={(e) => updateConfig("gateway.processingTimeout", parseInt(e.target.value) || 0)} />
            </div>
          </div>

          {/* Auth section */}
          {secHead(zh ? "\u8BA4\u8BC1" : "AUTHENTICATION")}
          <div style={fcard}>
            <div style={{ display: "flex", alignItems: "center", gap: 10, padding: "11px 15px", background: V.bg3, borderBottom: `1px solid ${V.bd}` }}>
              <div style={{ fontSize: 10, fontWeight: 600, color: V.t2, letterSpacing: 0.4, textTransform: "uppercase" as const, flex: 1 }}>Token {zh ? "\u8BA4\u8BC1" : "Auth"}</div>
              <Toggle on={!!getVal("gateway.auth.token", "")} onClick={() => { if (getVal("gateway.auth.token", "")) updateConfig("gateway.auth.token", ""); }} />
            </div>
            <div style={{ ...fieldRow, borderBottom: "none" }}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>Auth Token</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>gateway.auth.token</div>
              </div>
              <input style={{ ...fInput, minWidth: 240 }} type="password" value={getVal("gateway.auth.token", "")} placeholder="${RSCLAW_AUTH_TOKEN}" onChange={(e) => updateConfig("gateway.auth.token", e.target.value)} />
            </div>
          </div>

          {/* Agent defaults */}
          {secHead(zh ? "\u667A\u80FD\u4F53\u9ED8\u8BA4\u503C" : "AGENT DEFAULTS")}
          <div style={fcard}>
            <div style={fieldRow}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u4E0A\u4E0B\u6587\u538B\u7F29\u6A21\u5F0F" : "Compaction Mode"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>agents.defaults.compaction.mode</div>
              </div>
              <select style={{ ...fSelect, minWidth: 240 }} value={getVal("agents.defaults.compaction.mode", "layered")} onChange={(e) => updateConfig("agents.defaults.compaction.mode", e.target.value)}>
                <option value="layered">layered {zh ? "\u2014 \u5206\u5C42\u538B\u7F29\uFF08\u63A8\u8350\uFF09" : "-- layered (recommended)"}</option>
                <option value="default">default</option>
                <option value="safeguard">safeguard</option>
              </select>
            </div>
            <div style={fieldRow}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u4FDD\u7559\u6700\u8FD1\u5BF9\u8BDD\u8F6E\u6570" : "Keep Recent Pairs"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>agents.defaults.compaction.keepRecentPairs</div>
              </div>
              <input style={{ ...fInput, minWidth: 100 }} type="number" value={getVal("agents.defaults.compaction.keepRecentPairs", 5)} onChange={(e) => updateConfig("agents.defaults.compaction.keepRecentPairs", parseInt(e.target.value) || 5)} />
            </div>
            <div style={{ ...fieldRow, borderBottom: "none" }}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u8BF7\u6C42\u8D85\u65F6" : "Timeout"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>agents.defaults.timeoutSeconds ({zh ? "\u79D2" : "sec"})</div>
              </div>
              <input style={{ ...fInput, minWidth: 100 }} type="number" value={getVal("agents.defaults.timeoutSeconds", 600)} onChange={(e) => updateConfig("agents.defaults.timeoutSeconds", parseInt(e.target.value) || 600)} />
            </div>
          </div>
        </div>)}

        {/* ══ MODELS TAB ══ */}
        {activeTab === "models" && (<div style={{ animation: "fi .15s ease" }}>
          {secHead(zh ? "LLM \u63D0\u4F9B\u5546" : "LLM PROVIDERS")}
          <div style={{ fontSize: 11, color: V.t3, marginBottom: 12 }}>{zh ? "\u9009\u4E2D\u63D0\u4F9B\u5546\u586B\u5165 Key\uFF0C\u6D4B\u8BD5\u8FDE\u63A5\u6210\u529F\u540E\u4ECE API \u83B7\u53D6\u53EF\u7528\u6A21\u578B\u5217\u8868\uFF0C\u9009\u62E9\u9ED8\u8BA4\u6A21\u578B\u3002" : "Enter API Key per provider, test connection, then select a default model."}</div>

          {/* Provider cards */}
          <div style={{ display: "flex", flexDirection: "column", gap: 8, marginBottom: 24 }}>
            {provList.map((p) => {
              const isOpen = openProvs.has(p.id);
              const apiKey = getVal(`models.providers.${p.id}.apiKey`, "");
              const baseUrl = getVal(`models.providers.${p.id}.baseUrl`, "");
              const testSt = provTest[p.id] || "idle";
              const errMsg = provErr[p.id] || "";
              const models = provModels[p.id] || [];
              const selModel = provSelModel[p.id] || "";
              const hasKey = !!apiKey || p.isUrl;

              return (
                <div key={p.id} style={{ border: `1.5px solid ${testSt === "ok" ? "rgba(249,115,22,.28)" : V.bd2}`, borderRadius: 11, overflow: "hidden", background: V.bg2, transition: "border-color .13s" }}>
                  {/* Head */}
                  <div onClick={() => toggleProv(p.id)} style={{ display: "flex", alignItems: "center", gap: 12, padding: "13px 16px", cursor: "pointer" }}>
                    <div style={{ width: 32, height: 32, borderRadius: 8, background: testSt === "ok" ? V.olo : V.bg4, border: `1px solid ${testSt === "ok" ? V.obrd : V.bd2}`, display: "flex", alignItems: "center", justifyContent: "center", fontSize: 11, fontWeight: 700, color: testSt === "ok" ? V.or : V.t2, fontFamily: V.mono, flexShrink: 0, transition: "all .13s" }}>
                      {p.id.slice(0, 2).toUpperCase()}
                    </div>
                    <div style={{ flex: 1 }}>
                      <div style={{ fontSize: 13, fontWeight: 600, color: testSt === "ok" ? V.t0 : V.t1 }}>
                        {p.name}
                        {(zh ? p.tag : p.tagEn) && <span style={{ fontSize: 9, fontWeight: 500, color: V.t2, marginLeft: 4 }}>{zh ? p.tag : p.tagEn}</span>}
                      </div>
                      <div style={{ fontSize: 10, color: V.t3, marginTop: 2 }}>{testSt === "ok" ? (selModel || (zh ? "\u5DF2\u8FDE\u63A5" : "Connected")) : (zh ? "\u672A\u914D\u7F6E" : "Not configured")}</div>
                    </div>
                    <div style={{ display: "flex", alignItems: "center", gap: 8, flexShrink: 0 }}>
                      {testSt === "ok" ? (
                        <span style={{ fontSize: 9.5, padding: "2px 8px", borderRadius: 20, fontFamily: V.mono, fontWeight: 500, background: V.glo, color: V.green, border: `1px solid ${V.gbrd}` }}>{zh ? "\u2713 \u5DF2\u8FDE\u63A5" : "\u2713 Connected"}</span>
                      ) : (
                        <span style={{ fontSize: 9.5, padding: "2px 8px", borderRadius: 20, fontFamily: V.mono, fontWeight: 500, background: V.bg4, color: V.t2, border: `1px solid ${V.bd2}` }}>{zh ? "\u672A\u914D\u7F6E" : "Not configured"}</span>
                      )}
                      <Chevron open={isOpen} />
                    </div>
                  </div>
                  {/* Body */}
                  <div style={{ maxHeight: isOpen ? 600 : 0, overflow: "hidden", transition: "max-height .28s ease" }}>
                    <div style={{ padding: "0 16px 16px", borderTop: `1px solid ${V.bd}` }}>
                      <div style={{ paddingTop: 14 }}>
                        {/* Ollama: single Base URL field */}
                        {p.id === "ollama" ? (
                          <div style={{ marginBottom: 8 }}>
                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginBottom: 6 }}>Base URL</div>
                            <div style={{ display: "flex", gap: 8 }}>
                              <input
                                style={{ flex: 1, background: V.bg4, border: `1px solid ${testSt === "ok" ? V.green : testSt === "err" ? V.red : V.bd2}`, borderRadius: 7, padding: "8px 10px", color: V.t0, fontFamily: V.mono, fontSize: 11.5, outline: "none", transition: "border-color .12s" }}
                                type="text"
                                placeholder="http://localhost:11434"
                                value={baseUrl || "http://localhost:11434"}
                                onChange={(e) => {
                                  updateConfig(`models.providers.${p.id}.baseUrl`, e.target.value);
                                  setProvTest((prev) => ({ ...prev, [p.id]: "idle" }));
                                }}
                              />
                              <button onClick={() => handleTestProvider(p.id)} disabled={testSt === "testing"}
                                style={{ padding: "8px 14px", borderRadius: 7, border: `1px solid ${testSt === "ok" ? V.gbrd : testSt === "err" ? V.rbrd : testSt === "testing" ? V.obrd : V.bd2}`, background: testSt === "ok" ? V.glo : V.bg4, color: testSt === "ok" ? V.green : testSt === "err" ? V.red : testSt === "testing" ? V.or : V.t1, fontSize: 11, fontWeight: 500, cursor: testSt === "testing" ? "not-allowed" : "pointer", whiteSpace: "nowrap", flexShrink: 0, display: "flex", alignItems: "center", gap: 5, transition: "all .13s" }}>
                                {testSt === "testing" ? <><Spinner />{zh ? "\u8FDE\u63A5\u4E2D" : "Testing"}</> : testSt === "ok" ? (zh ? "\u2713 \u5DF2\u8FDE\u63A5" : "\u2713 Connected") : testSt === "err" ? (zh ? "\u91CD\u65B0\u6D4B\u8BD5" : "Retry") : (zh ? "\u6D4B\u8BD5\u8FDE\u63A5" : "Test")}
                              </button>
                            </div>
                          </div>
                        ) : p.id === "custom" ? (
                          /* Custom: API Type + Base URL + API Key */
                          <div style={{ marginBottom: 8 }}>
                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginBottom: 6 }}>API Type</div>
                            <select
                              style={{ width: "100%", background: V.bg4, border: `1px solid ${V.bd2}`, borderRadius: 7, padding: "8px 10px", color: V.t0, fontFamily: V.mono, fontSize: 11.5, outline: "none", cursor: "pointer", marginBottom: 8 }}
                              value={getVal(`models.providers.${p.id}.api`, "openai")}
                              onChange={(e) => {
                                updateConfig(`models.providers.${p.id}.api`, e.target.value);
                                setProvTest((prev) => ({ ...prev, [p.id]: "idle" }));
                              }}
                            >
                              {(Object.keys(API_TYPE_LABELS) as ApiType[]).map((at) => (
                                <option key={at} value={at}>{API_TYPE_LABELS[at]}</option>
                              ))}
                            </select>
                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginBottom: 6 }}>API URL</div>
                            <input
                              style={{ width: "100%", background: V.bg4, border: `1px solid ${V.bd2}`, borderRadius: 7, padding: "8px 10px", color: V.t0, fontFamily: V.mono, fontSize: 11.5, outline: "none", marginBottom: 8 }}
                              type="text"
                              placeholder="https://your-api-server.com"
                              value={baseUrl}
                              onChange={(e) => {
                                updateConfig(`models.providers.${p.id}.baseUrl`, e.target.value);
                                setProvTest((prev) => ({ ...prev, [p.id]: "idle" }));
                              }}
                            />
                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginBottom: 6 }}>API Key <span style={{ color: V.t3 }}>{zh ? "(\u53EF\u9009)" : "(optional)"}</span></div>
                            <div style={{ display: "flex", gap: 8 }}>
                              <input
                                style={{ flex: 1, background: V.bg4, border: `1px solid ${testSt === "ok" ? V.green : testSt === "err" ? V.red : V.bd2}`, borderRadius: 7, padding: "8px 10px", color: V.t0, fontFamily: V.mono, fontSize: 11.5, outline: "none", transition: "border-color .12s" }}
                                type="password"
                                placeholder="sk-..."
                                value={apiKey}
                                onChange={(e) => {
                                  updateConfig(`models.providers.${p.id}.apiKey`, e.target.value);
                                  setProvTest((prev) => ({ ...prev, [p.id]: "idle" }));
                                }}
                              />
                              <button onClick={() => handleTestProvider(p.id)} disabled={testSt === "testing"}
                                style={{ padding: "8px 14px", borderRadius: 7, border: `1px solid ${testSt === "ok" ? V.gbrd : testSt === "err" ? V.rbrd : testSt === "testing" ? V.obrd : V.bd2}`, background: testSt === "ok" ? V.glo : V.bg4, color: testSt === "ok" ? V.green : testSt === "err" ? V.red : testSt === "testing" ? V.or : V.t1, fontSize: 11, fontWeight: 500, cursor: testSt === "testing" ? "not-allowed" : "pointer", whiteSpace: "nowrap", flexShrink: 0, display: "flex", alignItems: "center", gap: 5, transition: "all .13s" }}>
                                {testSt === "testing" ? <><Spinner />{zh ? "\u8FDE\u63A5\u4E2D" : "Testing"}</> : testSt === "ok" ? (zh ? "\u2713 \u5DF2\u8FDE\u63A5" : "\u2713 Connected") : testSt === "err" ? (zh ? "\u91CD\u65B0\u6D4B\u8BD5" : "Retry") : (zh ? "\u6D4B\u8BD5\u8FDE\u63A5" : "Test")}
                              </button>
                            </div>
                          </div>
                        ) : (
                          /* Standard providers: API Key field */
                          <div style={{ marginBottom: 8 }}>
                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginBottom: 6 }}>{p.keyLabel}</div>
                            <div style={{ display: "flex", gap: 8 }}>
                              <input
                                style={{ flex: 1, background: V.bg4, border: `1px solid ${testSt === "ok" ? V.green : testSt === "err" ? V.red : V.bd2}`, borderRadius: 7, padding: "8px 10px", color: V.t0, fontFamily: V.mono, fontSize: 11.5, outline: "none", transition: "border-color .12s" }}
                                type="password"
                                placeholder={p.keyPlaceholder}
                                value={apiKey}
                                onChange={(e) => {
                                  updateConfig(`models.providers.${p.id}.apiKey`, e.target.value);
                                  setProvTest((prev) => ({ ...prev, [p.id]: "idle" }));
                                }}
                              />
                              <button onClick={() => handleTestProvider(p.id)} disabled={testSt === "testing"}
                                style={{ padding: "8px 14px", borderRadius: 7, border: `1px solid ${testSt === "ok" ? V.gbrd : testSt === "err" ? V.rbrd : testSt === "testing" ? V.obrd : V.bd2}`, background: testSt === "ok" ? V.glo : V.bg4, color: testSt === "ok" ? V.green : testSt === "err" ? V.red : testSt === "testing" ? V.or : V.t1, fontSize: 11, fontWeight: 500, cursor: testSt === "testing" ? "not-allowed" : "pointer", whiteSpace: "nowrap", flexShrink: 0, display: "flex", alignItems: "center", gap: 5, transition: "all .13s" }}>
                                {testSt === "testing" ? <><Spinner />{zh ? "\u8FDE\u63A5\u4E2D" : "Testing"}</> : testSt === "ok" ? (zh ? "\u2713 \u5DF2\u8FDE\u63A5" : "\u2713 Connected") : testSt === "err" ? (zh ? "\u91CD\u65B0\u6D4B\u8BD5" : "Retry") : (zh ? "\u6D4B\u8BD5\u8FDE\u63A5" : "Test")}
                              </button>
                            </div>
                          </div>
                        )}
                        {/* Error message */}
                        {testSt === "err" && errMsg && (
                          <div style={{ fontSize: 11, color: V.red, marginBottom: 8, padding: "6px 10px", background: V.rlo, border: `1px solid ${V.rbrd}`, borderRadius: 6 }}>{errMsg}</div>
                        )}
                        {/* Model list */}
                        {testSt === "ok" && models.length > 0 && (
                          <div style={{ marginTop: 12 }}>
                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.4, marginBottom: 8, display: "flex", alignItems: "center", justifyContent: "space-between" }}>
                              <span>{zh ? "\u9009\u62E9\u9ED8\u8BA4\u6A21\u578B" : "Select default model"}</span>
                              <span style={{ color: V.t3 }}>{models.length} {zh ? "\u4E2A\u53EF\u7528" : "available"}</span>
                            </div>
                            <div style={{ display: "flex", flexDirection: "column", gap: 4, maxHeight: 200, overflowY: "auto" }}>
                              {models.map((m) => {
                                const isSel = selModel === m.id || (!selModel && models[0]?.id === m.id);
                                const fallbackModel = (MODELS[p.id] || []).find((fm) => fm.id === m.id);
                                const tag = m.tag || (fallbackModel ? (zh ? fallbackModel.tag : fallbackModel.tagEn) : "");
                                const isRec = fallbackModel?.rec;
                                return (
                                  <div key={m.id} onClick={() => handleSelectModel(p.id, m.id)}
                                    style={{ display: "flex", alignItems: "center", gap: 9, padding: "7px 10px", borderRadius: 7, cursor: "pointer", border: `1px solid ${isSel ? V.obrd : "transparent"}`, background: isSel ? V.olo : "transparent", transition: "all .12s" }}>
                                    <div style={{ width: 14, height: 14, borderRadius: "50%", border: `1.5px solid ${isSel ? V.or : V.bg5}`, background: isSel ? V.or : "transparent", display: "flex", alignItems: "center", justifyContent: "center", flexShrink: 0, transition: "all .13s" }}>
                                      {isSel && <div style={{ width: 5, height: 5, borderRadius: "50%", background: "#fff" }} />}
                                    </div>
                                    <span style={{ fontFamily: V.mono, fontSize: 11, flex: 1, color: isSel ? V.t0 : V.t1 }}>{m.id}</span>
                                    {tag && <span style={{ fontSize: 9, padding: "1px 6px", borderRadius: 3, fontWeight: 500, background: isRec ? V.olo : V.bg4, color: isRec ? V.or : V.t2 }}>{tag}</span>}
                                  </div>
                                );
                              })}
                            </div>
                          </div>
                        )}
                      </div>
                    </div>
                  </div>
                </div>
              );
            })}
          </div>

          {/* Default model */}
          {secHead(zh ? "\u9ED8\u8BA4\u667A\u80FD\u4F53\u6A21\u578B" : "DEFAULT AGENT MODEL")}
          <div style={fcard}>
            <div style={fieldRow}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u4E3B\u6A21\u578B" : "Primary Model"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>agents.defaults.model.primary</div>
              </div>
              <input style={{ ...fInput, minWidth: 300 }} value={getVal("agents.defaults.model.primary", "")} onChange={(e) => updateConfig("agents.defaults.model.primary", e.target.value)} />
            </div>
            <div style={{ ...fieldRow, borderBottom: "none" }}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u5DE5\u5177\u96C6" : "Toolset"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>agents.defaults.model.toolset</div>
              </div>
              <select style={{ ...fSelect, minWidth: 240 }} value={getVal("agents.defaults.model.toolset", "full")} onChange={(e) => updateConfig("agents.defaults.model.toolset", e.target.value)}>
                <option value="minimal">minimal {zh ? "\u2014 6 \u4E2A\u6838\u5FC3\u5DE5\u5177" : "-- 6 core tools"}</option>
                <option value="standard">standard {zh ? "\u2014 12 \u4E2A\u5DE5\u5177" : "-- 12 tools"}</option>
                <option value="full">full {zh ? "\u2014 \u5168\u90E8\u5DE5\u5177" : "-- all tools"}</option>
              </select>
            </div>
          </div>
        </div>)}

        {/* ══ CHANNELS TAB (multi-account) ══ */}
        {activeTab === "channels" && (<div style={{ animation: "fi .15s ease" }}>
          {secHead(zh ? "\u6D88\u606F\u901A\u9053" : "CHANNELS")}
          <div style={{ fontSize: 11, color: V.t3, marginBottom: 12 }}>{zh ? "\u6BCF\u4E2A\u901A\u9053\u652F\u6301\u591A\u8D26\u53F7\u3002\u70B9\u51FB\u5C55\u5F00\u901A\u9053\u540E\u6DFB\u52A0\u8D26\u53F7\u5E76\u914D\u7F6E\u51ED\u8BC1\u3002" : "Each channel supports multiple accounts. Expand a channel to add accounts and configure credentials."}</div>

          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            {chList.map((c) => {
              const isOpen = openChs.has(c.id);
              const accounts = getChannelAccounts(c.id);
              const connCount = accounts.length;
              const fields = CRED_FIELDS[c.id] || [];

              return (
                <div key={c.id} style={{ border: `1.5px solid ${connCount > 0 ? "rgba(249,115,22,.22)" : V.bd2}`, borderRadius: 11, overflow: "hidden", background: V.bg2, transition: "border-color .13s" }}>
                  {/* Channel head */}
                  <div onClick={() => toggleCh(c.id)} style={{ display: "flex", alignItems: "center", gap: 12, padding: "12px 16px", cursor: "pointer" }}>
                    <div style={{ width: 30, height: 30, borderRadius: 7, display: "flex", alignItems: "center", justifyContent: "center", fontSize: 11, fontWeight: 700, color: connCount > 0 ? V.or : V.t3, background: connCount > 0 ? V.olo : V.bg4, border: `1px solid ${connCount > 0 ? V.obrd : V.bd}`, flexShrink: 0 }}>
                      {c.icon}
                    </div>
                    <div style={{ flex: 1 }}>
                      <div style={{ fontSize: 12, fontWeight: 600, color: connCount > 0 ? V.t0 : V.t1 }}>{zh ? c.name : c.nameEn}</div>
                      <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>
                        {connCount > 0 ? (zh ? `${connCount} \u4E2A\u8D26\u53F7` : `${connCount} account${connCount > 1 ? "s" : ""}`) : (zh ? "\u672A\u914D\u7F6E" : "Not configured")}
                      </div>
                    </div>
                    <div style={{ display: "flex", alignItems: "center", gap: 10, flexShrink: 0 }}>
                      {connCount > 0 ? (
                        <span style={{ fontSize: 9.5, padding: "2px 8px", borderRadius: 20, fontFamily: V.mono, fontWeight: 500, background: V.glo, color: V.green, border: `1px solid ${V.gbrd}` }}>{connCount} {zh ? "\u5DF2\u914D\u7F6E" : "configured"}</span>
                      ) : (
                        <span style={{ fontSize: 9.5, padding: "2px 8px", borderRadius: 20, fontFamily: V.mono, fontWeight: 500, background: V.bg4, color: V.t2, border: `1px solid ${V.bd2}` }}>{zh ? "\u672A\u914D\u7F6E" : "none"}</span>
                      )}
                      <Chevron open={isOpen} />
                    </div>
                  </div>

                  {/* Channel body - account list */}
                  <div style={{ maxHeight: isOpen ? 2000 : 0, overflow: "hidden", transition: "max-height .28s ease" }}>
                    <div style={{ padding: "14px 16px 16px", borderTop: `1px solid ${V.bd}` }}>
                      {accounts.length === 0 && (
                        <div style={{ textAlign: "center", padding: "16px 0 8px", color: V.t3, fontSize: 12 }}>{zh ? "\u6682\u65E0\u8D26\u53F7" : "No accounts yet"}</div>
                      )}

                      {accounts.map((acct) => {
                        const aKey = `${c.id}-${acct.id}`;
                        const aOpen = openAccts[aKey] || false;
                        const aTab = acctTab[aKey] || (c.hasQr ? "qr" : "cred");
                        const aPath = acct.id === "default" ? `channels.${c.id}` : `channels.${c.id}.accounts.${acct.id}`;
                        const acctLabel = acct.id === "default" ? getVal(`${aPath}.label`, "") : getVal(`${aPath}.label`, "");
                        const hasAnyCred = fields.some((f) => getVal(`${aPath}.${f.key}`, ""));

                        return (
                          <div key={acct.id} style={{ background: V.bg3, border: `1px solid ${hasAnyCred ? "rgba(45,212,160,.18)" : V.bd}`, borderRadius: 9, marginBottom: 8, overflow: "hidden" }}>
                            {/* Account head */}
                            <div onClick={() => toggleAcct(c.id, acct.id)} style={{ display: "flex", alignItems: "center", gap: 10, padding: "11px 14px", cursor: "pointer" }}>
                              <div style={{ width: 8, height: 8, borderRadius: "50%", background: hasAnyCred ? V.green : V.bg5, flexShrink: 0 }} />
                              <div style={{ flex: 1, minWidth: 0 }}>
                                <div style={{ fontSize: 12, fontWeight: 600, color: hasAnyCred ? V.t0 : V.t1 }}>{acctLabel || acct.id}</div>
                                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 1 }}>
                                  {hasAnyCred ? (fields.map((f) => getVal(`${aPath}.${f.key}`, "")).filter(Boolean)[0] || acct.id) : (zh ? "\u672A\u914D\u7F6E" : "not configured")}
                                </div>
                              </div>
                              <div style={{ display: "flex", alignItems: "center", gap: 8, flexShrink: 0 }}>
                                {hasAnyCred ? (
                                  <span style={{ fontSize: 9, padding: "2px 7px", borderRadius: 20, fontFamily: V.mono, fontWeight: 500, background: V.glo, color: V.green, border: `1px solid ${V.gbrd}` }}>{c.hasQr ? (zh ? "\u5DF2\u8FDE\u63A5" : "connected") : (zh ? "\u5DF2\u914D\u7F6E" : "configured")}</span>
                                ) : (
                                  <span style={{ fontSize: 9, padding: "2px 7px", borderRadius: 20, fontFamily: V.mono, fontWeight: 500, background: V.bg4, color: V.t2, border: `1px solid ${V.bd2}` }}>{zh ? "\u672A\u914D\u7F6E" : "idle"}</span>
                                )}
                                <Chevron open={aOpen} />
                              </div>
                            </div>

                            {/* Account body */}
                            <div style={{ maxHeight: aOpen ? 520 : 0, overflow: "hidden", transition: "max-height .28s ease" }}>
                              <div style={{ padding: "0 14px 14px", borderTop: `1px solid ${V.bd}` }}>
                                {/* Account ID + Label inputs */}
                                <div style={{ paddingTop: 12, display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8, marginBottom: 10 }}>
                                  <div>
                                    <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 4 }}>{zh ? "\u8D26\u53F7 ID" : "Account ID"}</div>
                                    <input style={{ ...fInput, width: "100%", minWidth: 0, fontSize: 11 }} type="text" value={acct.id} readOnly />
                                  </div>
                                  <div>
                                    <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 4 }}>{zh ? "\u5907\u6CE8\u540D" : "Label"}</div>
                                    <input style={{ ...fInput, width: "100%", minWidth: 0, fontSize: 11 }} type="text" value={acctLabel} placeholder={zh ? "\u4E3B\u53F7\u3001\u5DE5\u4F5C\u53F7..." : "main, work..."} onChange={(e) => updateConfig(`${aPath}.label`, e.target.value)} />
                                  </div>
                                </div>

                                {/* QR / Credential tabs for QR-capable channels */}
                                {c.hasQr ? (
                                  <>
                                    <div style={{ display: "flex", gap: 0, borderBottom: `1px solid ${V.bd}`, margin: "0 -14px", padding: "0 14px" }}>
                                      <button onClick={() => setAcctTab((prev) => ({ ...prev, [aKey]: "qr" }))}
                                        style={{ padding: "8px 14px", fontSize: 11, fontWeight: 500, color: aTab === "qr" ? V.or : V.t2, cursor: "pointer", background: "none", border: "none", borderBottom: `2px solid ${aTab === "qr" ? V.or : "transparent"}`, marginBottom: -1, transition: "all .13s" }}>
                                        {zh ? "\u626B\u7801" : "QR Scan"}
                                      </button>
                                      <button onClick={() => setAcctTab((prev) => ({ ...prev, [aKey]: "cred" }))}
                                        style={{ padding: "8px 14px", fontSize: 11, fontWeight: 500, color: aTab === "cred" ? V.or : V.t2, cursor: "pointer", background: "none", border: "none", borderBottom: `2px solid ${aTab === "cred" ? V.or : "transparent"}`, marginBottom: -1, transition: "all .13s" }}>
                                        {zh ? "\u51ED\u8BC1" : "Credentials"}
                                      </button>
                                    </div>
                                    {/* QR pane */}
                                    {aTab === "qr" && (
                                      <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 10, padding: "14px 0" }}>
                                        <div style={{ width: 130, height: 130, background: V.bg4, border: `1px solid ${V.bd2}`, borderRadius: 10, display: "flex", alignItems: "center", justifyContent: "center", overflow: "hidden" }}>
                                          {qrData[aKey] || qrData[c.id] ? (
                                            <img src={qrData[aKey] || qrData[c.id]} alt="QR" style={{ width: 110, height: 110 }} />
                                          ) : (
                                            <div style={{ color: V.t3, fontSize: 11 }}>{zh ? "\u70B9\u51FB\u83B7\u53D6" : "Click to get QR"}</div>
                                          )}
                                        </div>
                                        <div style={{ fontSize: 11, color: V.t2, textAlign: "center", lineHeight: 1.5 }}>
                                          {zh ? `\u4F7F\u7528${c.name.split("/")[0].trim()}\u626B\u7801\u767B\u5F55` : `Scan with ${c.nameEn.split("/")[0].trim()}`}
                                        </div>
                                        {(qrStatus[aKey] || qrStatus[c.id]) && (
                                          <div style={{ fontSize: 11, color: V.t3, display: "flex", alignItems: "center", gap: 5 }}>
                                            <Spinner color="green" />
                                            {qrStatus[aKey] || qrStatus[c.id]}
                                          </div>
                                        )}
                                        <button onClick={() => handleQrStart(c.id)}
                                          style={{ fontSize: 10, color: V.or, cursor: "pointer", fontFamily: V.mono, background: V.olo, border: `1px solid ${V.obrd}`, padding: "4px 12px", borderRadius: 5, transition: "all .12s" }}>
                                          {(qrData[aKey] || qrData[c.id]) ? (zh ? "\u5237\u65B0\u4E8C\u7EF4\u7801" : "Refresh QR") : (zh ? "\u83B7\u53D6\u4E8C\u7EF4\u7801" : "Get QR Code")}
                                        </button>
                                      </div>
                                    )}
                                    {/* Credential pane */}
                                    {aTab === "cred" && (
                                      <div style={{ paddingTop: 10 }}>
                                        {fields.map((f) => (
                                          <div key={f.key} style={{ marginBottom: 10 }}>
                                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 4 }}>{f.label}</div>
                                            <input style={{ ...fInput, width: "100%", minWidth: 0 }} type={f.type} placeholder={f.ph} value={getVal(`${aPath}.${f.key}`, "")} onChange={(e) => updateConfig(`${aPath}.${f.key}`, e.target.value)} />
                                          </div>
                                        ))}
                                        {/* Brand select for feishu */}
                                        {c.id === "feishu" && (
                                          <div style={{ marginBottom: 10 }}>
                                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 4 }}>{zh ? "\u54C1\u724C" : "Brand"}</div>
                                            <select style={{ ...fSelect, width: "100%", minWidth: 0 }} value={getVal(`${aPath}.brand`, "\u98DE\u4E66")} onChange={(e) => updateConfig(`${aPath}.brand`, e.target.value)}>
                                              <option value="\u98DE\u4E66">{zh ? "\u98DE\u4E66" : "Feishu"}</option>
                                              <option value="Lark">Lark</option>
                                            </select>
                                          </div>
                                        )}
                                      </div>
                                    )}
                                  </>
                                ) : (
                                  /* Non-QR channels: credential fields directly */
                                  <div style={{ paddingTop: 4 }}>
                                    {fields.map((f) => (
                                      <div key={f.key} style={{ marginBottom: 10 }}>
                                        <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 4 }}>{f.label}</div>
                                        <input style={{ ...fInput, width: "100%", minWidth: 0 }} type={f.type} placeholder={f.ph} value={getVal(`${aPath}.${f.key}`, "")} onChange={(e) => updateConfig(`${aPath}.${f.key}`, e.target.value)} />
                                      </div>
                                    ))}
                                    {fields.length === 0 && (
                                      <div style={{ fontSize: 11, color: V.t3, padding: "10px 0" }}>{zh ? "\u6B64\u901A\u9053\u65E0\u989D\u5916\u51ED\u8BC1\u5B57\u6BB5" : "No credential fields for this channel"}</div>
                                    )}
                                  </div>
                                )}

                                {/* Delete button at bottom */}
                                <div style={{ display: "flex", justifyContent: "space-between", marginTop: 12, paddingTop: 10, borderTop: `1px solid ${V.bd}` }}>
                                  <div style={{ position: "relative", display: "inline-block" }}>
                                    <button onClick={() => setOpenAccts((prev) => ({ ...prev, [`del-${c.id}-${acct.id}`]: !prev[`del-${c.id}-${acct.id}`] }))}
                                      style={{ fontSize: 11, padding: "5px 12px", borderRadius: 5, cursor: "pointer", background: V.rlo, color: V.red, border: `1px solid ${V.rbrd}`, fontFamily: V.mono, transition: "all .12s" }}>
                                      {zh ? "\u5220\u9664" : "Delete"}
                                    </button>
                                    {openAccts[`del-${c.id}-${acct.id}`] && (
                                      <div onClick={(e) => e.stopPropagation()} style={{
                                        position: "absolute", bottom: "100%", right: 0, marginBottom: 6,
                                        padding: "10px 12px", minWidth: 160,
                                        background: "var(--white)", border: "1px solid var(--border-in-light)",
                                        borderRadius: 8, boxShadow: "0 4px 12px rgba(0,0,0,0.3)", zIndex: 100,
                                      }}>
                                        <div style={{ fontSize: 11, color: "var(--black)", marginBottom: 8 }}>
                                          {zh ? `\u786E\u8BA4\u5220\u9664\u8D26\u53F7 ${acctLabel || acct.id}\uFF1F` : `Delete account ${acctLabel || acct.id}?`}
                                        </div>
                                        <div style={{ display: "flex", gap: 6, justifyContent: "flex-end" }}>
                                          <button onClick={() => setOpenAccts((prev) => ({ ...prev, [`del-${c.id}-${acct.id}`]: false }))}
                                            style={{ fontSize: 10, padding: "3px 10px", borderRadius: 5, border: "1px solid var(--border-in-light)", background: "transparent", color: "var(--black)", cursor: "pointer" }}>
                                            {zh ? "\u53D6\u6D88" : "Cancel"}
                                          </button>
                                          <button onClick={() => { removeAccount(c.id, acct.id); setOpenAccts((prev) => ({ ...prev, [`del-${c.id}-${acct.id}`]: false })); }}
                                            style={{ fontSize: 10, padding: "3px 10px", borderRadius: 5, border: "none", cursor: "pointer", fontWeight: 600, background: "#d95f5f", color: "#fff" }}>
                                            {zh ? "\u5220\u9664" : "Delete"}
                                          </button>
                                        </div>
                                      </div>
                                    )}
                                  </div>
                                </div>
                              </div>
                            </div>
                          </div>
                        );
                      })}

                      {/* Add account button */}
                      <button onClick={() => addAccount(c.id)}
                        style={{ width: "100%", padding: "10px 0", borderRadius: 8, cursor: "pointer", background: V.bg4, color: V.t1, border: `1px dashed ${V.bd2}`, fontSize: 12, fontWeight: 500, fontFamily: V.sans, marginTop: accounts.length > 0 ? 4 : 0, transition: "all .12s" }}>
                        <span style={{ fontSize: 16, lineHeight: "1", verticalAlign: "middle", marginRight: 4 }}>+</span>
                        {zh ? "\u6DFB\u52A0\u8D26\u53F7" : "Add Account"}
                      </button>

                      {/* Channel-level policies (shared across all accounts) */}
                      {accounts.length > 0 && (
                        <div style={{ marginTop: 12, padding: "12px 14px", background: V.bg3, border: `1px solid ${V.bd}`, borderRadius: 9 }}>
                          <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 8, textTransform: "uppercase" }}>
                            {zh ? "\u901A\u9053\u7B56\u7565" : "Channel Policies"}
                          </div>
                          {/* DM Policy */}
                          <div style={{ marginBottom: 10 }}>
                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 4 }}>dmPolicy</div>
                            <select style={{ ...fSelect, width: "100%", minWidth: 0 }} value={getVal(`channels.${c.id}.dmPolicy`, "pairing")} onChange={(e) => updateConfig(`channels.${c.id}.dmPolicy`, e.target.value)}>
                              {DM_POLICIES.map((p) => <option key={p} value={p}>{p}</option>)}
                            </select>
                          </div>
                          {/* allowFrom - shown when dmPolicy=allowlist */}
                          {getVal(`channels.${c.id}.dmPolicy`, "pairing") === "allowlist" && (
                            <div style={{ marginBottom: 10 }}>
                              <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 4 }}>
                                allowFrom <span style={{ color: V.t3, fontWeight: 400 }}>{zh ? "(\u6BCF\u884C\u4E00\u4E2A\u7528\u6237 ID)" : "(one user ID per line)"}</span>
                              </div>
                              <textarea
                                style={{ ...fInput, width: "100%", minWidth: 0, minHeight: 60, resize: "vertical", fontFamily: V.mono, fontSize: 11, lineHeight: 1.6 }}
                                placeholder={zh ? "\u7528\u6237ID1\n\u7528\u6237ID2\n..." : "user_id_1\nuser_id_2\n..."}
                                value={(getVal(`channels.${c.id}.allowFrom`, []) as string[]).join("\n")}
                                onChange={(e) => {
                                  const ids = e.target.value.split("\n").map((s: string) => s.trim()).filter(Boolean);
                                  updateConfig(`channels.${c.id}.allowFrom`, ids);
                                }}
                              />
                            </div>
                          )}
                          {/* Group Policy */}
                          <div style={{ marginBottom: 10 }}>
                            <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 4 }}>groupPolicy</div>
                            <select style={{ ...fSelect, width: "100%", minWidth: 0 }} value={getVal(`channels.${c.id}.groupPolicy`, "allowlist")} onChange={(e) => updateConfig(`channels.${c.id}.groupPolicy`, e.target.value)}>
                              {["allowlist", "open", "disabled"].map((p) => <option key={p} value={p}>{p}</option>)}
                            </select>
                          </div>
                          {/* groupAllowFrom - shown when groupPolicy=allowlist */}
                          {getVal(`channels.${c.id}.groupPolicy`, "allowlist") === "allowlist" && (
                            <div>
                              <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, letterSpacing: 0.3, marginBottom: 4 }}>
                                groupAllowFrom <span style={{ color: V.t3, fontWeight: 400 }}>{zh ? "(\u6BCF\u884C\u4E00\u4E2A\u7FA4 ID)" : "(one group ID per line)"}</span>
                              </div>
                              <textarea
                                style={{ ...fInput, width: "100%", minWidth: 0, minHeight: 60, resize: "vertical", fontFamily: V.mono, fontSize: 11, lineHeight: 1.6 }}
                                placeholder={zh ? "\u7FA4ID1\n\u7FA4ID2\n..." : "group_id_1\ngroup_id_2\n..."}
                                value={(getVal(`channels.${c.id}.groupAllowFrom`, []) as string[]).join("\n")}
                                onChange={(e) => {
                                  const ids = e.target.value.split("\n").map((s: string) => s.trim()).filter(Boolean);
                                  updateConfig(`channels.${c.id}.groupAllowFrom`, ids);
                                }}
                              />
                            </div>
                          )}
                        </div>
                      )}
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        </div>)}

        {/* ══ TOOLS TAB ══ */}
        {activeTab === "tools" && (<div style={{ animation: "fi .15s ease" }}>
          {/* Exec sandbox */}
          {secHead(zh ? "\u6267\u884C\u5DE5\u5177" : "EXEC TOOLS")}
          <div style={fcard}>
            <div style={{ ...fieldRow, borderBottom: "none" }}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>exec {zh ? "\u5B89\u5168\u6C99\u7BB1" : "Sandbox"}</div>
                <div style={{ fontSize: 10, color: V.t3, marginTop: 2, lineHeight: 1.5 }}>{zh ? "\u9650\u5236\u53EF\u6267\u884C\u547D\u4EE4\u8303\u56F4\uFF0C\u9632\u6B62\u5371\u9669\u64CD\u4F5C" : "Restrict executable commands to prevent dangerous operations"}</div>
              </div>
              <Toggle on={getVal("tools.exec.sandbox", false)} onClick={() => updateConfig("tools.exec.sandbox", !getVal("tools.exec.sandbox", false))} />
            </div>
          </div>

          {/* File upload */}
          {secHead(zh ? "\u6587\u4EF6\u4E0A\u4F20" : "FILE UPLOAD")}
          <div style={fcard}>
            <div style={fieldRow}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u6700\u5927\u6587\u4EF6\u5927\u5C0F" : "Max File Size"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>tools.upload.maxFileSize</div>
              </div>
              <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                <input style={{ ...fInput, minWidth: 80 }} type="number" value={getVal("tools.upload.maxFileSize", 50)} onChange={(e) => updateConfig("tools.upload.maxFileSize", parseInt(e.target.value) || 50)} />
                <span style={{ fontSize: 11, color: V.t2 }}>MB</span>
              </div>
            </div>
            <div style={{ ...fieldRow, borderBottom: "none" }}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u6587\u672C\u6700\u5927\u5B57\u7B26\u6570" : "Max Text Chars"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>tools.upload.maxTextChars</div>
              </div>
              <input style={{ ...fInput, minWidth: 140 }} type="number" value={getVal("tools.upload.maxTextChars", 20000)} onChange={(e) => updateConfig("tools.upload.maxTextChars", parseInt(e.target.value) || 20000)} />
            </div>
          </div>

          {/* Web search */}
          {secHead(zh ? "\u7F51\u7EDC\u641C\u7D22" : "WEB SEARCH")}
          <div style={fcard}>
            <div style={{ display: "flex", alignItems: "center", gap: 10, padding: "11px 15px", background: V.bg3, borderBottom: `1px solid ${V.bd}` }}>
              <div style={{ fontSize: 10, fontWeight: 600, color: V.t2, letterSpacing: 0.4, textTransform: "uppercase" as const, flex: 1 }}>webSearch</div>
              <Toggle on={getVal("tools.webSearch.enabled", true)} onClick={() => updateConfig("tools.webSearch.enabled", !getVal("tools.webSearch.enabled", true))} />
            </div>
            <div style={{ ...fieldRow, borderBottom: "none" }}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u641C\u7D22\u63D0\u4F9B\u5546" : "Search Provider"}</div>
              </div>
              <select style={{ ...fSelect, minWidth: 260 }} value={getVal("tools.webSearch.provider", "bing-free")} onChange={(e) => updateConfig("tools.webSearch.provider", e.target.value)}>
                <option value="bing-free">Bing {zh ? "(\u514D\u8D39)" : "(free)"}</option>
                <option value="baidu-free">{zh ? "\u767E\u5EA6 (\u514D\u8D39)" : "Baidu (free)"}</option>
                <option value="sogou">{zh ? "\u641C\u72D7 (\u514D\u8D39)" : "Sogou (free)"}</option>
                <option value="360">{zh ? "360\u641C\u7D22 (\u514D\u8D39)" : "360 Search (free)"}</option>
                <option value="duckduckgo">DuckDuckGo {zh ? "(\u514D\u8D39)" : "(free)"}</option>
                <option value="google">Google {zh ? "(\u9700 API Key)" : "(API key)"}</option>
                <option value="bing">Bing {zh ? "(\u9700 API Key)" : "(API key)"}</option>
                <option value="brave">Brave {zh ? "(\u9700 API Key)" : "(API key)"}</option>
                <option value="baidu">{zh ? "\u767E\u5EA6 (\u9700 API Key)" : "Baidu (API key)"}</option>
              </select>
            </div>
          </div>

          {/* Memory */}
          {secHead(zh ? "\u957F\u671F\u8BB0\u5FC6" : "MEMORY")}
          <div style={fcard}>
            <div style={fieldRow}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "\u6BCF\u4E2A\u540E\u7AEF\u53EC\u56DE\u6570" : "Recall Top K"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>memory.recallTopK</div>
              </div>
              <input style={{ ...fInput, minWidth: 100 }} type="number" value={getVal("memory.recallTopK", 10)} onChange={(e) => updateConfig("memory.recallTopK", parseInt(e.target.value) || 10)} />
            </div>
            <div style={{ ...fieldRow, borderBottom: "none" }}>
              <div style={{ flex: 1 }}>
                <div style={{ fontSize: 12, color: V.t1, fontWeight: 500 }}>{zh ? "RRF \u878D\u5408\u540E\u8FD4\u56DE\u6570" : "Recall Final K"}</div>
                <div style={{ fontSize: 10, color: V.t3, fontFamily: V.mono, marginTop: 2 }}>memory.recallFinalK</div>
              </div>
              <input style={{ ...fInput, minWidth: 100 }} type="number" value={getVal("memory.recallFinalK", 5)} onChange={(e) => updateConfig("memory.recallFinalK", parseInt(e.target.value) || 5)} />
            </div>
          </div>
        </div>)}

        {/* ══ RAW JSON5 TAB ══ */}
        {activeTab === "raw" && (
          <div style={{ height: "100%", animation: "fi .15s ease" }}>
            <div style={{ fontSize: 11, color: V.t3, marginBottom: 8, fontFamily: V.mono }}>{cfgPath}</div>
            <textarea value={raw} spellCheck={false}
              onChange={(e) => {
                setRaw(e.target.value); setDirty(true);
                try { setConfig(JSON.parse(e.target.value)); setParseError(""); } catch { setParseError(zh ? "JSON \u683C\u5F0F\u9519\u8BEF" : "Invalid JSON"); }
              }}
              style={{ width: "100%", height: "calc(100% - 50px)", background: V.bg1, border: `1px solid ${V.bd}`, borderRadius: 10, padding: "14px 16px", color: V.t0, fontFamily: V.mono, fontSize: 12, lineHeight: 1.6, outline: "none", resize: "none" }} />
            {parseError && <div style={{ fontSize: 11, color: V.red, marginTop: 6, padding: "6px 10px", background: V.rlo, border: `1px solid ${V.rbrd}`, borderRadius: 6 }}>{parseError}</div>}
          </div>
        )}
      </div>
    </div>
  );
}

function TauriConfigPage() {
  return <ErrorBoundary><TauriConfigPageInner /></ErrorBoundary>;
}

// ══════════════════════════════════════════════════════════
// ── Cron Task Page ───────────────────────────────────────
// ══════════════════════════════════════════════════════════

interface CronJob {
  id: string;
  name: string;
  agentId?: string;
  enabled: boolean;
  schedule?: { kind: string; expr: string; tz?: string };
  payload?: { kind: string; text?: string };
  sessionKey?: string;
  sessionTarget?: string;
  last_run?: string;
  next_run?: string;
}

const CRON_TEMPLATES = [
  { label: { cn: "\u6BCF\u5929 09:00", en: "Daily 09:00" }, cron: "0 9 * * *" },
  { label: { cn: "\u6BCF\u5C0F\u65F6", en: "Every hour" }, cron: "0 * * * *" },
  { label: { cn: "\u6BCF\u5468\u4E00", en: "Mon 09:00" }, cron: "0 9 * * 1" },
  { label: { cn: "\u6BCF\u6708 1 \u53F7", en: "1st monthly" }, cron: "0 9 1 * *" },
  { label: { cn: "\u6BCF 15 \u5206\u949F", en: "Every 15min" }, cron: "*/15 * * * *" },
  { label: { cn: "\u5DE5\u4F5C\u65E5 09:00", en: "Weekdays 09:00" }, cron: "0 9 * * 1-5" },
];

function cronToHuman(expr: string): string {
  const zh = getLang() === "cn";
  const parts = expr.trim().split(/\s+/);
  if (parts.length !== 5) return "";
  const [min, hour, dom, , dow] = parts;
  if (expr === "0 * * * *") return zh ? "\u6BCF\u5C0F\u65F6\u6574\u70B9" : "Every hour";
  if (expr.startsWith("*/")) return zh ? `\u6BCF ${parts[0].slice(2)} \u5206\u949F` : `Every ${parts[0].slice(2)} minutes`;
  const timeStr = `${hour.padStart(2, "0")}:${min.padStart(2, "0")}`;
  if (dom !== "*") return zh ? `\u6BCF\u6708 ${dom} \u53F7 ${timeStr}` : `${dom}th monthly ${timeStr}`;
  const days: Record<string, string> = zh
    ? { "0": "\u5468\u65E5", "1": "\u5468\u4E00", "2": "\u5468\u4E8C", "3": "\u5468\u4E09", "4": "\u5468\u56DB", "5": "\u5468\u4E94", "6": "\u5468\u516D", "1-5": "\u5DE5\u4F5C\u65E5" }
    : { "0": "Sun", "1": "Mon", "2": "Tue", "3": "Wed", "4": "Thu", "5": "Fri", "6": "Sat", "1-5": "Weekdays" };
  if (dow !== "*") return `${days[dow] || dow} ${timeStr}`;
  return zh ? `\u6BCF\u5929 ${timeStr}` : `Daily ${timeStr}`;
}

function CronTaskPage() {
  const zh = getLang() === "cn";
  const [jobs, setJobs] = useState<CronJob[]>([]);
  const [loading, setLoading] = useState(true);
  const [cronEnabled, setCronEnabled] = useState(true);
  const [showForm, setShowForm] = useState(false);
  const [editJob, setEditJob] = useState<CronJob | null>(null);
  const [form, setForm] = useState({ name: "", schedule: "", message: "", agentId: "", enabled: true });
  const [agents, setAgents] = useState<{ id: string; name?: string }[]>([]);
  const [runningId, setRunningId] = useState<string | null>(null);

  const fetchJobs = useCallback(async () => {
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) {
        const data: any = await tauriInvoke("get_cron_jobs");
        setJobs(data.jobs || []);
      } else {
        const res = await gatewayFetch("/api/v1/cron");
        if (res.ok) { const data = await res.json(); setJobs(data.jobs || []); }
      }
    } catch {}
    setLoading(false);
  }, []);

  const fetchAgents = useCallback(async () => {
    try { const data = await getAgents(); setAgents(Array.isArray(data) ? data : data.agents || []); } catch {}
  }, []);

  useEffect(() => { fetchJobs(); fetchAgents(); }, [fetchJobs, fetchAgents]);

  const saveJob = async () => {
    try {
      const invoke = (window as any).__TAURI__?.invoke;
      if (invoke) {
        const data: any = await invoke("get_cron_jobs");
        const existing = data.jobs || [];
        const jobObj: any = {
          id: editJob?.id || crypto.randomUUID(),
          name: form.name,
          agentId: form.agentId || "main",
          enabled: form.enabled,
          schedule: { kind: "cron", expr: form.schedule, tz: Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC" },
          payload: { kind: "systemEvent", text: form.message },
        };
        let updated: any[];
        if (editJob) {
          const idx = existing.findIndex((j: any) => j.id === editJob.id);
          if (idx >= 0) { existing[idx] = { ...existing[idx], ...jobObj }; updated = existing; }
          else { updated = [...existing, jobObj]; }
        } else {
          updated = [...existing, jobObj];
        }
        await invoke("save_cron_jobs", { content: JSON.stringify({ version: data.version || 1, jobs: updated }, null, 2) });
      } else {
        // Fallback to gateway API
        if (editJob) {
          await gatewayFetch(`/api/v1/cron/${editJob.id}`, { method: "PUT", body: JSON.stringify(form) });
        } else {
          await gatewayFetch("/api/v1/cron", { method: "POST", body: JSON.stringify(form) });
        }
      }
      setShowForm(false); setEditJob(null); fetchJobs();
    } catch {}
  };

  const deleteJob = async (id: string) => {
    try {
      const invoke = (window as any).__TAURI__?.invoke;
      if (invoke) {
        const data: any = await invoke("get_cron_jobs");
        const jobs = (data.jobs || []).filter((j: any) => j.id !== id);
        await invoke("save_cron_jobs", { content: JSON.stringify({ version: data.version || 1, jobs }, null, 2) });
      } else {
        await gatewayFetch(`/api/v1/cron/${id}`, { method: "DELETE" });
      }
      fetchJobs();
    } catch {}
  };
  const triggerJob = async (id: string) => {
    try {
      // Try gateway API for triggering (needs running gateway to execute)
      await gatewayFetch(`/api/v1/cron/${id}/trigger`, { method: "POST" });
    } catch {}
  };
  const toggleJob = async (job: CronJob) => {
    try {
      const invoke = (window as any).__TAURI__?.invoke;
      if (invoke) {
        const data: any = await invoke("get_cron_jobs");
        const jobs = data.jobs || [];
        const idx = jobs.findIndex((j: any) => j.id === job.id);
        if (idx >= 0) {
          jobs[idx].enabled = !jobs[idx].enabled;
          await invoke("save_cron_jobs", { content: JSON.stringify({ version: data.version || 1, jobs }, null, 2) });
        }
      }
      fetchJobs();
    } catch {}
  };

  const openEdit = (job: CronJob) => {
    setEditJob(job);
    setForm({ name: job.name, schedule: job.schedule?.expr || "", message: job.payload?.text || "", agentId: job.agentId || "", enabled: job.enabled });
    setShowForm(true);
  };
  const openNew = () => {
    setEditJob(null);
    setForm({ name: "", schedule: "", message: "", agentId: agents[0]?.id || "", enabled: true });
    setShowForm(true);
  };

  const V2 = { bg2: "#141618", bg3: "#1a1c22", bg4: "#1f2126", bg5: "#252830", bd: "rgba(255,255,255,.055)", bd2: "rgba(255,255,255,.09)", t0: "#eceaf4", t1: "#9896a4", t2: "#4a4858", t3: "#2e2c3a", or: "#f97316", olo: "rgba(249,115,22,.09)", obrd: "rgba(249,115,22,.2)", green: "#2dd4a0", gbrd: "rgba(45,212,160,.18)", red: "#d95f5f", rbrd: "rgba(217,95,95,.18)", mono: "'JetBrains Mono', monospace" };

  return (
    <div style={{ flex: 1, overflow: "hidden", display: "flex", flexDirection: "column" }}>
      {/* Header */}
      <div style={{ padding: "24px 28px 0", display: "flex", alignItems: "flex-start", justifyContent: "space-between", flexShrink: 0 }}>
        <div>
          <div style={{ fontSize: 20, fontWeight: 700, color: V2.t0, letterSpacing: -0.4 }}>{zh ? "\u5B9A\u65F6\u4EFB\u52A1" : "Cron Tasks"}</div>
          <div style={{ fontSize: 11, color: V2.t3, fontFamily: V2.mono, marginTop: 3 }}>~/.rsclaw/cron/jobs.json</div>
        </div>
        <button onClick={openNew} style={{ padding: "8px 16px", borderRadius: 9, border: "none", background: V2.or, color: "#fff", fontSize: 12, fontWeight: 700, boxShadow: "0 2px 8px rgba(249,115,22,.28)", cursor: "pointer", display: "flex", alignItems: "center", gap: 6 }}>
          <span>+</span> {zh ? "\u65B0\u5EFA\u4EFB\u52A1" : "New Task"}
        </button>
      </div>

      <div style={{ padding: "20px 28px 28px", flex: 1, overflowY: "auto" }}>
        {/* Global switch */}
        <div style={{ display: "flex", alignItems: "center", gap: 16, padding: "12px 16px", background: V2.bg2, border: `1px solid ${V2.bd}`, borderRadius: 10, marginBottom: 20 }}>
          <div style={{ flex: 1 }}>
            <div style={{ fontSize: 12, fontWeight: 500, color: V2.t1 }}>{zh ? "\u5B9A\u65F6\u4EFB\u52A1\u603B\u5F00\u5173" : "Cron Master Switch"}</div>
            <div style={{ fontSize: 10, color: V2.t3, fontFamily: V2.mono, marginTop: 2 }}>cron.enabled</div>
          </div>
          <Toggle checked={cronEnabled} onChange={() => setCronEnabled(!cronEnabled)} />
        </div>

        {/* Job list */}
        {loading ? <div style={{ textAlign: "center", color: V2.t3, padding: 40 }}>...</div>
        : jobs.length === 0 ? (
          <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 10, padding: "40px 0", color: V2.t3 }}>
            <div style={{ fontSize: 32, opacity: 0.4 }}>&#x23F0;</div>
            <div style={{ fontSize: 12 }}>{zh ? "\u6682\u65E0\u5B9A\u65F6\u4EFB\u52A1" : "No cron tasks"}</div>
          </div>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            {jobs.map((job) => {
              const expr = job.schedule?.expr || "";
              const msg = job.payload?.text || "";
              return (
                <div key={job.id} style={{ background: V2.bg2, border: `1px solid ${V2.bd}`, borderRadius: 11, opacity: job.enabled ? 1 : 0.7, transition: "border-color .13s" }}>
                  <div style={{ display: "flex", alignItems: "center", gap: 12, padding: "14px 16px" }}>
                    <div style={{ width: 8, height: 8, borderRadius: "50%", background: job.enabled ? V2.green : V2.bg5, flexShrink: 0 }} />
                    <div style={{ flex: 1, minWidth: 0 }}>
                      <div style={{ fontSize: 13, fontWeight: 600, color: "#eceaf4" }}>{job.name || job.id}</div>
                      <div style={{ display: "flex", alignItems: "center", gap: 10, marginTop: 4 }}>
                        {expr && <span style={{ fontFamily: V2.mono, fontSize: 11, color: "#f97316", background: "rgba(249,115,22,0.12)", border: `1px solid ${V2.obrd}`, padding: "1px 7px", borderRadius: 4 }}>{expr}</span>}
                        <span style={{ fontSize: 11, color: "rgba(255,255,255,0.45)" }}>{cronToHuman(expr)}</span>
                        {job.agentId && <span style={{ fontSize: 11, color: "rgba(255,255,255,0.45)", fontFamily: V2.mono }}>{"-> "}{job.agentId}</span>}
                      </div>
                      {msg && <div style={{ fontSize: 10, color: "rgba(255,255,255,0.38)", marginTop: 3, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap", maxWidth: 300 }}>{msg.slice(0, 50)}</div>}
                    </div>
                    <div style={{ textAlign: "right", flexShrink: 0, marginRight: 8 }}>
                      {job.next_run && <div style={{ fontSize: 10, color: "rgba(255,255,255,0.45)" }}>{zh ? "\u4E0B\u6B21" : "Next"}: {job.next_run}</div>}
                      {!job.enabled && <span style={{ fontSize: 10, padding: "2px 8px", borderRadius: 20, background: "rgba(255,255,255,0.06)", color: "rgba(255,255,255,0.4)", border: "1px solid rgba(255,255,255,0.1)" }}>{zh ? "\u5DF2\u6682\u505C" : "Paused"}</span>}
                    </div>
                    <div style={{ display: "flex", alignItems: "center", gap: 6, flexShrink: 0 }}>
                      <button onClick={() => { setRunningId(job.id); triggerJob(job.id); setTimeout(() => setRunningId(null), 1500); }}
                        style={{ padding: "4px 9px", borderRadius: 6, border: "1px solid rgba(45,212,160,0.3)", background: "rgba(45,212,160,0.07)", color: "#2dd4a0", fontSize: 10, fontWeight: 500, cursor: "pointer" }}>
                        {runningId === job.id ? "..." : (zh ? "\u7ACB\u5373\u8FD0\u884C" : "Run")}
                      </button>
                      <button onClick={() => openEdit(job)}
                        style={{ padding: "4px 9px", borderRadius: 6, border: `1px solid ${V2.bd2}`, background: "transparent", color: V2.t2, fontSize: 10, fontWeight: 500, cursor: "pointer" }}>
                        {zh ? "\u7F16\u8F91" : "Edit"}
                      </button>
                      <button onClick={() => { if (window.confirm(zh ? `\u786E\u8BA4\u5220\u9664 "${job.name || job.id}"\uFF1F` : `Delete "${job.name || job.id}"?`)) deleteJob(job.id); }}
                        style={{ padding: "4px 9px", borderRadius: 6, border: "1px solid rgba(217,95,95,.2)", background: "transparent", color: "rgba(217,95,95,.7)", fontSize: 10, fontWeight: 500, cursor: "pointer" }}>
                        {zh ? "\u5220\u9664" : "Delete"}
                      </button>
                      <Toggle checked={job.enabled} onChange={() => toggleJob(job)} />
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>

      {showForm && (
        <div style={{ position: "fixed", inset: 0, background: "rgba(0,0,0,0.5)", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 1000 }}
          onClick={(e) => { if (e.target === e.currentTarget) { setShowForm(false); setEditJob(null); } }}>
          <div style={{ width: 460, background: "#1a1c22", border: "1px solid rgba(255,255,255,.09)", borderRadius: 14, overflow: "hidden", boxShadow: "0 20px 60px rgba(0,0,0,.6)" }}>
            <div style={{ padding: "20px 22px 0", display: "flex", alignItems: "center", justifyContent: "space-between" }}>
              <div style={{ fontSize: 15, fontWeight: 700, color: "#eceaf4" }}>
                {editJob ? (zh ? "\u7F16\u8F91\u4EFB\u52A1" : "Edit Task") : (zh ? "\u65B0\u5EFA\u4EFB\u52A1" : "New Task")}
              </div>
              <button onClick={() => { setShowForm(false); setEditJob(null); }} style={{ width: 26, height: 26, borderRadius: "50%", border: "1px solid rgba(255,255,255,.09)", background: "transparent", color: "#4a4858", fontSize: 14, display: "flex", alignItems: "center", justifyContent: "center", cursor: "pointer" }}>{"\u2715"}</button>
            </div>
            <div style={{ padding: "18px 22px" }}>
              <div style={{ marginBottom: 14 }}>
                <div style={{ fontSize: 10, color: "#2e2c3a", letterSpacing: 0.4, marginBottom: 5, fontFamily: "'JetBrains Mono', monospace" }}>{zh ? "\u4EFB\u52A1\u540D\u79F0" : "TASK NAME"}</div>
                <input value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} placeholder={zh ? "\u6BCF\u65E5\u65E9\u62A5" : "Daily report"}
                  style={{ width: "100%", padding: "8px 10px", borderRadius: 7, border: "1px solid rgba(255,255,255,.09)", background: "#1f2126", color: "#eceaf4", fontFamily: "'JetBrains Mono', monospace", fontSize: 11.5, outline: "none" }} />
              </div>
              <div style={{ marginBottom: 14 }}>
                <div style={{ fontSize: 10, color: "#2e2c3a", letterSpacing: 0.4, marginBottom: 5, fontFamily: "'JetBrains Mono', monospace" }}>CRON {zh ? "\u8868\u8FBE\u5F0F" : "EXPRESSION"}</div>
                <input value={form.schedule} onChange={(e) => setForm({ ...form, schedule: e.target.value })} placeholder="0 9 * * 1-5"
                  style={{ width: "100%", padding: "8px 10px", borderRadius: 7, border: "1px solid rgba(255,255,255,.09)", background: "#1f2126", color: "#eceaf4", fontFamily: "'JetBrains Mono', monospace", fontSize: 11.5, outline: "none" }} />
                <div style={{ display: "flex", gap: 6, marginTop: 8, flexWrap: "wrap" }}>
                  {CRON_TEMPLATES.map((tpl) => (
                    <button key={tpl.cron} onClick={() => setForm({ ...form, schedule: tpl.cron })}
                      style={{ padding: "3px 9px", borderRadius: 5, border: "1px solid rgba(255,255,255,.09)", background: "transparent", color: form.schedule === tpl.cron ? "#f97316" : "#4a4858", fontSize: 10, fontFamily: "'JetBrains Mono', monospace", cursor: "pointer" }}>
                      {zh ? tpl.label.cn : tpl.label.en}
                    </button>
                  ))}
                </div>
                <div style={{ fontSize: 10, color: form.schedule && cronToHuman(form.schedule) ? "#2dd4a0" : "transparent", fontFamily: "'JetBrains Mono', monospace", marginTop: 4, minHeight: 14 }}>
                  {cronToHuman(form.schedule) || ""}
                </div>
              </div>
              <div style={{ marginBottom: 14 }}>
                <div style={{ fontSize: 10, color: "#2e2c3a", letterSpacing: 0.4, marginBottom: 5, fontFamily: "'JetBrains Mono', monospace" }}>{zh ? "\u76EE\u6807\u667A\u80FD\u4F53" : "TARGET AGENT"}</div>
                <select value={form.agentId} onChange={(e) => setForm({ ...form, agentId: e.target.value })}
                  style={{ width: "100%", padding: "8px 10px", borderRadius: 7, border: "1px solid rgba(255,255,255,.09)", background: "#1f2126", color: "#eceaf4", fontSize: 12, outline: "none", cursor: "pointer" }}>
                  <option value="">--</option>
                  {agents.map((a) => <option key={a.id} value={a.id}>{a.name || a.id}</option>)}
                </select>
              </div>
              <div style={{ marginBottom: 0 }}>
                <div style={{ fontSize: 10, color: "#2e2c3a", letterSpacing: 0.4, marginBottom: 5, fontFamily: "'JetBrains Mono', monospace" }}>{zh ? "\u89E6\u53D1\u6D88\u606F" : "TRIGGER MESSAGE"}</div>
                <textarea value={form.message} onChange={(e) => setForm({ ...form, message: e.target.value })} rows={3}
                  style={{ width: "100%", padding: "8px 10px", borderRadius: 7, border: "1px solid rgba(255,255,255,.09)", background: "#1f2126", color: "#eceaf4", fontFamily: "'JetBrains Mono', monospace", fontSize: 11.5, outline: "none", resize: "vertical", minHeight: 72 }} />
              </div>
            </div>
            <div style={{ padding: "0 22px 20px", display: "flex", justifyContent: "flex-end", gap: 8 }}>
              <button onClick={() => { setShowForm(false); setEditJob(null); }} style={{ padding: "8px 16px", borderRadius: 8, border: "1px solid rgba(255,255,255,.09)", background: "#141618", color: "#4a4858", fontSize: 12, fontWeight: 500, cursor: "pointer" }}>{zh ? "\u53D6\u6D88" : "Cancel"}</button>
              <button onClick={saveJob} disabled={!form.name || !form.schedule}
                style={{ padding: "8px 18px", borderRadius: 8, border: "none", background: "#f97316", color: "#fff", fontSize: 12, fontWeight: 700, cursor: "pointer", opacity: form.name && form.schedule ? 1 : 0.4, boxShadow: "0 2px 8px rgba(249,115,22,.25)" }}>
                {zh ? "\u4FDD\u5B58\u4EFB\u52A1" : "Save Task"}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ══════════════════════════════════════════════════════════
// ── Skills Page ──────────────────────────────────────────
// ══════════════════════════════════════════════════════════

interface SkillInfo { name: string; description?: string; version?: string; author?: string; tools?: { name: string; description?: string }[]; path?: string; icon?: string; }

const RECOMMENDED_SKILLS: { name: string; icon: string; ver: string; author: string; desc: { cn: string; en: string }; tools: string[]; downloads?: string; stars?: string }[] = [
  { name: "self-improving-agent", icon: "\uD83E\uDDE0", ver: "v3.0.13", author: "@pskoett", desc: { cn: "\u6355\u83B7\u5B66\u4E60\u3001\u9519\u8BEF\u548C\u4FEE\u6B63\uFF0C\u5B9E\u73B0\u6301\u7EED\u6539\u8FDB", en: "Captures learnings, errors, and corrections for continuous improvement" }, tools: [], downloads: "345k", stars: "2.9k" },
  { name: "ontology", icon: "\uD83D\uDD17", ver: "v1.0.4", author: "@oswalpalash", desc: { cn: "\u7ED3\u6784\u5316\u667A\u80FD\u4F53\u8BB0\u5FC6\u77E5\u8BC6\u56FE\u8C31", en: "Typed knowledge graph for structured agent memory and composable skills" }, tools: [], downloads: "150k", stars: "472" },
  { name: "Self-Improving-Proactive-Agent", icon: "\u2728", ver: "v1.2.16", author: "@ivangdavila", desc: { cn: "\u81EA\u53CD\u601D+\u81EA\u6279\u8BC4+\u81EA\u5B66\u4E60+\u81EA\u7EC4\u7EC7\u8BB0\u5FC6", en: "Self-reflection, self-criticism, self-learning, self-organizing memory" }, tools: [], downloads: "144k", stars: "866" },
  { name: "AdMapix", icon: "\uD83D\uDCCA", ver: "v1.0.28", author: "@fly0pants", desc: { cn: "\u5E7F\u544A\u60C5\u62A5\u4E0E\u5E94\u7528\u5206\u6790\u52A9\u624B", en: "Ad intelligence & app analytics assistant" }, tools: [], downloads: "78.9k", stars: "212" },
  { name: "nano-banana-pro", icon: "\uD83C\uDF4C", ver: "v1.0.1", author: "@steipete", desc: { cn: "Gemini 3 Pro \u56FE\u50CF\u751F\u6210/\u7F16\u8F91\uFF0C\u652F\u6301 1K/2K/4K", en: "Generate/edit images with Nano Banana Pro (Gemini 3 Pro Image)" }, tools: [], downloads: "77.5k", stars: "308" },
  { name: "obsidian", icon: "\uD83D\uDCDD", ver: "v1.0.0", author: "@steipete", desc: { cn: "Obsidian \u77E5\u8BC6\u5E93\u64CD\u4F5C\u4E0E\u81EA\u52A8\u5316", en: "Work with Obsidian vaults and automate via obsidian-cli" }, tools: [], downloads: "73.1k", stars: "297" },
  { name: "baidu-search", icon: "\uD83D\uDD0D", ver: "v1.1.3", author: "@ide-rea", desc: { cn: "\u767E\u5EA6 AI \u641C\u7D22\u5F15\u64CE", en: "Search the web using Baidu AI Search Engine" }, tools: [], downloads: "71.5k", stars: "188" },
  { name: "Agent-Browser", icon: "\uD83C\uDF10", ver: "v0.1.0", author: "@matrixy", desc: { cn: "\u65E0\u5934\u6D4F\u89C8\u5668\u81EA\u52A8\u5316 CLI\uFF0C\u4F18\u5316 AI \u4EA4\u4E92", en: "Headless browser automation CLI optimized for AI agents" }, tools: [], downloads: "67k", stars: "236" },
  { name: "api-gateway", icon: "\uD83D\uDD0C", ver: "v1.0.76", author: "@byungkyu", desc: { cn: "\u8FDE\u63A5 100+ API\uFF08Google, Microsoft, GitHub, Notion, Slack \u7B49\uFF09", en: "Connect to 100+ APIs with managed OAuth" }, tools: [], downloads: "61.9k", stars: "303" },
  { name: "mcporter", icon: "\uD83D\uDEE0\uFE0F", ver: "v1.0.0", author: "@steipete", desc: { cn: "MCP \u670D\u52A1\u5668/\u5DE5\u5177\u7BA1\u7406 CLI", en: "List, configure, auth, and call MCP servers/tools directly" }, tools: [], downloads: "51.3k", stars: "156" },
  { name: "free-ride", icon: "\uD83C\uDD93", ver: "v1.0.8", author: "@shaivpidadi", desc: { cn: "\u514D\u8D39 AI \u6A21\u578B\u7BA1\u7406\uFF0C\u81EA\u52A8\u6392\u540D\u548C fallback", en: "Manages free AI models from OpenRouter with auto-ranking and fallbacks" }, tools: [], downloads: "50.9k", stars: "363" },
  { name: "prismfy-search", icon: "\uD83D\uDD0E", ver: "v1.1.0", author: "@uroboros1205", desc: { cn: "10 \u5F15\u64CE\u7F51\u7EDC\u641C\u7D22\uFF08Google, Reddit, GitHub, arXiv \u7B49\uFF09", en: "Search across 10 engines: Google, Reddit, GitHub, arXiv, Hacker News" }, tools: [], downloads: "49.3k", stars: "16" },
  { name: "word-docx", icon: "\uD83D\uDCC4", ver: "v1.0.2", author: "@ivangdavila", desc: { cn: "Word/DOCX \u6587\u6863\u521B\u5EFA\u3001\u68C0\u67E5\u548C\u7F16\u8F91", en: "Create, inspect, and edit Microsoft Word documents and DOCX files" }, tools: [], downloads: "48.2k", stars: "222" },
  { name: "excel-xlsx", icon: "\uD83D\uDCCA", ver: "v1.0.2", author: "@ivangdavila", desc: { cn: "Excel/XLSX \u5DE5\u4F5C\u7C3F\u521B\u5EFA\u3001\u68C0\u67E5\u548C\u7F16\u8F91", en: "Create, inspect, and edit Microsoft Excel workbooks and XLSX files" }, tools: [], downloads: "42.9k", stars: "173" },
  { name: "imap-smtp-email", icon: "\uD83D\uDCE7", ver: "v0.0.10", author: "community", desc: { cn: "IMAP/SMTP \u90AE\u4EF6\u6536\u53D1", en: "Email via IMAP/SMTP" }, tools: [], downloads: "40k", stars: "" },
];

function SkillsPage() {
  const zh = getLang() === "cn";
  const [installed, setInstalled] = useState<SkillInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [installing, setInstalling] = useState<string | null>(null);
  const [detailSkill, setDetailSkill] = useState<SkillInfo | null>(null);
  const [search, setSearch] = useState("");
  const [searchResults, setSearchResults] = useState<{ name: string; version?: string; description?: string }[]>([]);
  const [searching, setSearching] = useState(false);

  const fetchSkills = useCallback(async () => {
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) {
        const data: any = await tauriInvoke("get_skills");
        const skills = (data?.skills || []).map((s: any) => ({
          ...s,
          tools: (s.tools || []).map((t: string) => ({ name: t })),
        }));
        setInstalled(skills);
      } else {
        const res = await gatewayFetch("/api/v1/skills"); if (res.ok) { const data = await res.json(); setInstalled(data.skills || []); }
      }
    } catch {}
    setLoading(false);
  }, []);

  useEffect(() => { fetchSkills(); }, [fetchSkills]);

  const doInstall = async (name: string) => {
    setInstalling(name);
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) { await tauriInvoke("install_skill", { name }); }
      else { await gatewayFetch("/api/v1/skills/install", { method: "POST", body: JSON.stringify({ name }) }); }
      await fetchSkills();
      // Trigger gateway config reload so new skill is active
      try { await reloadConfig(); } catch {}
      toast.success(zh ? `${name} \u5B89\u88C5\u5B8C\u6210` : `${name} installed`);
    } catch (e: any) {
      const msg = typeof e === "string" ? e : e?.message || "";
      toast.fromError(zh ? "\u5B89\u88C5\u5931\u8D25" : "Install failed", msg);
    }
    setInstalling(null);
  };
  const doUninstall = async (name: string) => {
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) { await tauriInvoke("uninstall_skill", { name }); }
      else { await gatewayFetch(`/api/v1/skills/${encodeURIComponent(name)}`, { method: "DELETE" }); }
      await fetchSkills(); setDetailSkill(null);
      try { await reloadConfig(); } catch {}
      toast.success(zh ? `${name} \u5DF2\u5378\u8F7D` : `${name} uninstalled`);
    } catch {}
  };

  const doSearch = async () => {
    if (!search.trim()) { setSearchResults([]); return; }
    setSearching(true);
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) {
        const data: any = await tauriInvoke("search_skills", { query: search.trim() });
        setSearchResults(data?.results || []);
      }
    } catch {}
    setSearching(false);
  };

  const V2 = { bg2: "#141618", bg3: "#1a1c22", bg4: "#1f2126", bg5: "#252830", bd: "rgba(255,255,255,.055)", bd2: "rgba(255,255,255,.09)", t0: "#eceaf4", t1: "#9896a4", t2: "#4a4858", t3: "#2e2c3a", or: "#f97316", olo: "rgba(249,115,22,.09)", obrd: "rgba(249,115,22,.2)", green: "#2dd4a0", glo: "rgba(45,212,160,.07)", gbrd: "rgba(45,212,160,.18)", red: "#d95f5f", rlo: "rgba(217,95,95,.08)", rbrd: "rgba(217,95,95,.18)", mono: "'JetBrains Mono', monospace" };
  const isInstalled = (name: string) => installed.some((s) => s.name === name);
  const filtered = RECOMMENDED_SKILLS.filter((s) => !search || s.name.includes(search.toLowerCase()));

  return (
    <div style={{ flex: 1, overflow: "hidden", display: "flex", flexDirection: "column" }}>
      <div style={{ padding: "24px 28px 0", flexShrink: 0 }}>
        <div style={{ fontSize: 20, fontWeight: 700, color: V2.t0, letterSpacing: -0.4 }}>{zh ? "\u6280\u80FD\u7BA1\u7406" : "Skills"}</div>
        <div style={{ fontSize: 11, color: V2.t3, fontFamily: V2.mono, marginTop: 3 }}>~/.rsclaw/skills/</div>
      </div>

      <div style={{ padding: "20px 28px 28px", flex: 1, overflowY: "auto" }}>
        {/* Search */}
        <div style={{ marginBottom: 20 }}>
          <input value={search} onChange={(e) => setSearch(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Enter") doSearch(); }}
            placeholder={zh ? "\u641C\u7D22\u6280\u80FD\uFF08\u56DE\u8F66\u8054\u7F51\u641C\u7D22\uFF09..." : "Search skills (Enter to search online)..."}
            style={{ width: "100%", background: V2.bg2, border: `1px solid ${V2.bd}`, borderRadius: 9, padding: "9px 14px", color: V2.t0, fontSize: 12, outline: "none" }} />
        </div>

        {/* Installed */}
        <div style={{ fontSize: 11, fontWeight: 600, color: V2.t2, letterSpacing: 0.5, textTransform: "uppercase", marginBottom: 10, display: "flex", alignItems: "center", gap: 8 }}>
          {zh ? "\u5DF2\u5B89\u88C5" : "Installed"} <span style={{ fontSize: 9, padding: "1px 7px", borderRadius: 3, background: V2.bg4, color: V2.t2 }}>{installed.length}</span>
        </div>
        {loading ? <div style={{ color: V2.t3, padding: 20, textAlign: "center" }}>...</div>
        : installed.length === 0 ? (
          <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 10, padding: "30px 0", color: V2.t3, marginBottom: 24 }}>
            <div style={{ fontSize: 32, opacity: 0.4 }}>{"\uD83D\uDD27"}</div>
            <div style={{ fontSize: 12 }}>{zh ? "\u5C1A\u672A\u5B89\u88C5\u4EFB\u4F55\u6280\u80FD" : "No skills installed"}</div>
          </div>
        ) : (
          <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8, marginBottom: 24 }}>
            {installed.map((skill) => (
              <div key={skill.name} onClick={() => setDetailSkill(skill)} style={{ background: V2.bg2, border: `1px solid rgba(45,212,160,.15)`, borderRadius: 11, padding: "14px 16px", cursor: "pointer", display: "flex", flexDirection: "column", gap: 10, transition: "border-color .13s" }}>
                <div style={{ display: "flex", alignItems: "flex-start", gap: 10 }}>
                  <div style={{ width: 36, height: 36, borderRadius: 9, display: "flex", alignItems: "center", justifyContent: "center", fontSize: 18, flexShrink: 0, background: V2.bg3, border: `1px solid ${V2.bd}` }}>{skill.icon || "\uD83D\uDCE6"}</div>
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div style={{ fontSize: 13, fontWeight: 600, color: V2.t0 }}>{skill.name}</div>
                    <div style={{ fontSize: 10, fontFamily: V2.mono, color: V2.t3, marginTop: 1 }}>{skill.version} {skill.author && `\u00B7 ${skill.author}`}</div>
                  </div>
                  <div style={{ fontSize: 10, color: V2.green, fontFamily: V2.mono, display: "flex", alignItems: "center", gap: 4 }}>{"●"} {zh ? "\u5DF2\u5B89\u88C5" : "Installed"}</div>
                </div>
                {skill.description && <div style={{ fontSize: 11, color: V2.t2, lineHeight: 1.55 }}>{skill.description}</div>}
                <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
                  <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
                    {skill.tools?.map((t) => <span key={t.name} style={{ fontSize: 9, padding: "1px 6px", borderRadius: 3, background: V2.bg4, color: V2.t2, fontFamily: V2.mono }}>{t.name}</span>)}
                  </div>
                  <button onClick={(e) => { e.stopPropagation(); doUninstall(skill.name); }} style={{ padding: "5px 12px", borderRadius: 7, border: `1px solid ${V2.rbrd}`, background: V2.rlo, color: V2.red, fontSize: 11, fontWeight: 600, cursor: "pointer" }}>{zh ? "\u5378\u8F7D" : "Uninstall"}</button>
                </div>
              </div>
            ))}
          </div>
        )}

        {/* Search results */}
        {searchResults.length > 0 && (
          <>
            <div style={{ fontSize: 11, fontWeight: 600, color: V2.t2, letterSpacing: 0.5, textTransform: "uppercase", marginBottom: 10, display: "flex", alignItems: "center", gap: 8 }}>
              {zh ? "\u641C\u7D22\u7ED3\u679C" : "Search Results"} <span style={{ fontSize: 9, padding: "1px 7px", borderRadius: 3, background: V2.bg4, color: V2.t2 }}>{searchResults.length}</span>
            </div>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8, marginBottom: 24 }}>
              {searchResults.map((sr) => (
                <div key={sr.name} style={{ background: V2.bg2, border: `1px solid ${V2.bd}`, borderRadius: 11, padding: "14px 16px", display: "flex", flexDirection: "column", gap: 10 }}>
                  <div>
                    <div style={{ fontSize: 13, fontWeight: 600, color: V2.t0 }}>{sr.name}</div>
                    <div style={{ fontSize: 10, fontFamily: V2.mono, color: V2.t3, marginTop: 1 }}>{sr.version}</div>
                  </div>
                  {sr.description && <div style={{ fontSize: 11, color: V2.t2, lineHeight: 1.55 }}>{sr.description}</div>}
                  <div style={{ display: "flex", justifyContent: "flex-end" }}>
                    {isInstalled(sr.name)
                      ? <span style={{ fontSize: 10, color: V2.green, fontFamily: V2.mono }}>{"●"} {zh ? "\u5DF2\u5B89\u88C5" : "Installed"}</span>
                      : <button onClick={() => doInstall(sr.name)} disabled={installing === sr.name}
                          style={{ padding: "5px 12px", borderRadius: 7, border: `1px solid ${installing === sr.name ? V2.obrd : V2.gbrd}`, background: installing === sr.name ? V2.olo : V2.glo, color: installing === sr.name ? V2.or : V2.green, fontSize: 11, fontWeight: 600, cursor: installing === sr.name ? "not-allowed" : "pointer" }}>
                          {installing === sr.name ? (zh ? "\u5B89\u88C5\u4E2D..." : "Installing...") : (zh ? "\u5B89\u88C5" : "Install")}
                        </button>}
                  </div>
                </div>
              ))}
            </div>
          </>
        )}
        {searching && <div style={{ textAlign: "center", color: V2.t3, padding: 20 }}>...</div>}

        {/* Recommended */}
        <div style={{ fontSize: 11, fontWeight: 600, color: V2.t2, letterSpacing: 0.5, textTransform: "uppercase", marginBottom: 10, display: "flex", alignItems: "center", gap: 8 }}>
          {zh ? "\u63A8\u8350\u5B89\u88C5" : "Recommended"} <span style={{ fontSize: 9, padding: "1px 7px", borderRadius: 3, background: V2.bg4, color: V2.t2 }}>{filtered.length}</span>
        </div>
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: 8, maxHeight: 420, overflowY: "auto" }}>
          {filtered.map((rec) => (
            <div key={rec.name} style={{ background: V2.bg2, border: `1px solid ${V2.bd}`, borderRadius: 11, padding: "14px 16px", display: "flex", flexDirection: "column", gap: 10, transition: "border-color .13s" }}>
              <div style={{ display: "flex", alignItems: "flex-start", gap: 10 }}>
                <div style={{ width: 36, height: 36, borderRadius: 9, display: "flex", alignItems: "center", justifyContent: "center", fontSize: 18, flexShrink: 0, background: V2.bg3, border: `1px solid ${V2.bd}` }}>{rec.icon}</div>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ fontSize: 13, fontWeight: 600, color: V2.t0 }}>{rec.name}</div>
                  <div style={{ fontSize: 10, fontFamily: V2.mono, color: V2.t3, marginTop: 1 }}>{rec.ver} {"\u00B7"} {rec.author}{rec.downloads ? ` \u00B7 ${rec.downloads}` : ""}{rec.stars ? ` \u2605 ${rec.stars}` : ""}</div>
                </div>
                {isInstalled(rec.name) && <div style={{ fontSize: 10, color: V2.green, fontFamily: V2.mono, display: "flex", alignItems: "center", gap: 4 }}>{"●"} {zh ? "\u5DF2\u5B89\u88C5" : "Installed"}</div>}
              </div>
              <div style={{ fontSize: 11, color: V2.t2, lineHeight: 1.55 }}>{zh ? rec.desc.cn : rec.desc.en}</div>
              <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
                <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
                  {rec.tools.map((t) => <span key={t} style={{ fontSize: 9, padding: "1px 6px", borderRadius: 3, background: V2.bg4, color: V2.t2, fontFamily: V2.mono }}>{t}</span>)}
                </div>
                {isInstalled(rec.name)
                  ? <span style={{ padding: "5px 12px", borderRadius: 7, border: `1px solid ${V2.bd2}`, color: V2.t2, fontSize: 11 }}>{"\u2713"}</span>
                  : <button onClick={() => doInstall(rec.name)} disabled={installing === rec.name}
                      style={{ padding: "5px 12px", borderRadius: 7, border: `1px solid ${installing === rec.name ? V2.obrd : V2.gbrd}`, background: installing === rec.name ? V2.olo : V2.glo, color: installing === rec.name ? V2.or : V2.green, fontSize: 11, fontWeight: 600, cursor: installing === rec.name ? "not-allowed" : "pointer" }}>
                      {installing === rec.name ? (zh ? "\u5B89\u88C5\u4E2D..." : "Installing...") : (zh ? "\u5B89\u88C5" : "Install")}
                    </button>}
              </div>
            </div>
          ))}
        </div>
      </div>

      {/* Detail modal */}
      {detailSkill && (
        <div style={{ position: "fixed", inset: 0, background: "rgba(5,5,7,.72)", backdropFilter: "blur(3px)", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 100 }}
          onClick={(e) => { if (e.target === e.currentTarget) setDetailSkill(null); }}>
          <div style={{ width: 460, background: V2.bg3, border: `1px solid ${V2.bd2}`, borderRadius: 14, overflow: "hidden", boxShadow: "0 20px 60px rgba(0,0,0,.6)" }}>
            <div style={{ padding: "20px 22px 0", display: "flex", alignItems: "center", justifyContent: "space-between" }}>
              <div style={{ fontSize: 15, fontWeight: 700 }}>{detailSkill.name}</div>
              <button onClick={() => setDetailSkill(null)} style={{ width: 26, height: 26, borderRadius: "50%", border: `1px solid ${V2.bd2}`, background: "transparent", color: V2.t2, fontSize: 14, display: "flex", alignItems: "center", justifyContent: "center", cursor: "pointer" }}>{"\u2715"}</button>
            </div>
            <div style={{ padding: "18px 22px" }}>
              <div style={{ fontSize: 10, fontFamily: V2.mono, color: V2.t3, marginBottom: 12 }}>{detailSkill.version} {detailSkill.author && `\u00B7 ${detailSkill.author}`}</div>
              {detailSkill.description && <div style={{ fontSize: 12, color: V2.t1, lineHeight: 1.6, marginBottom: 12 }}>{detailSkill.description}</div>}
              {detailSkill.tools && detailSkill.tools.length > 0 && (
                <div style={{ marginBottom: 12 }}>
                  <div style={{ fontSize: 10, color: V2.t3, letterSpacing: 0.4, marginBottom: 6, fontFamily: V2.mono }}>TOOLS</div>
                  <div style={{ display: "flex", gap: 4, flexWrap: "wrap" }}>
                    {detailSkill.tools.map((t) => <span key={t.name} style={{ fontSize: 9, padding: "2px 8px", borderRadius: 4, background: V2.bg4, color: V2.t2, fontFamily: V2.mono }}>{t.name}</span>)}
                  </div>
                </div>
              )}
              {detailSkill.path && <div style={{ fontSize: 10, color: V2.t3, fontFamily: V2.mono }}>{detailSkill.path}</div>}
            </div>
            <div style={{ padding: "0 22px 20px", display: "flex", justifyContent: "flex-end", gap: 8 }}>
              <button onClick={() => doUninstall(detailSkill.name)} style={{ padding: "8px 16px", borderRadius: 8, border: `1px solid ${V2.rbrd}`, background: V2.rlo, color: V2.red, fontSize: 12, fontWeight: 500, cursor: "pointer" }}>{zh ? "\u5378\u8F7D" : "Uninstall"}</button>
              <button onClick={() => setDetailSkill(null)} style={{ padding: "8px 16px", borderRadius: 8, border: `1px solid ${V2.bd2}`, background: V2.bg2, color: V2.t2, fontSize: 12, fontWeight: 500, cursor: "pointer" }}>{zh ? "\u5173\u95ED" : "Close"}</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ══════════════════════════════════════════════════════════
// ── Main RsClaw Panel ────────────────────────────────────
// ══════════════════════════════════════════════════════════

// ══════════════════════════════════════════════════════════
// ── Pairing Page ─────────────────────────────────────────
// ══════════════════════════════════════════════════════════

function PairingPage() {
  const zh = getLang() === "cn";
  const [pending, setPending] = useState<{ channel: string; peerId: string; code: string; ttlSeconds: number }[]>([]);
  const [approved, setApproved] = useState<{ channel: string; peerId: string }[]>([]);
  const [loading, setLoading] = useState(true);

  const fetchPairings = useCallback(async () => {
    try {
      const res = await gatewayFetch("/api/v1/channels/pairings");
      if (res.ok) {
        const data = await res.json();
        setPending(data.pending || []);
        setApproved(data.approved || []);
      }
    } catch {}
    setLoading(false);
  }, []);

  useEffect(() => { fetchPairings(); const iv = setInterval(fetchPairings, 5000); return () => clearInterval(iv); }, [fetchPairings]);

  const handleApprove = async (code: string) => {
    try {
      await gatewayFetch("/api/v1/channels/pair", { method: "POST", body: JSON.stringify({ code }) });
      toast.success(zh ? "\u5DF2\u901A\u8FC7" : "Approved");
      fetchPairings();
    } catch (e: any) {
      toast.fromError(zh ? "\u64CD\u4F5C\u5931\u8D25" : "Action failed", e);
    }
  };

  const handleRevoke = async (channel: string, peerId: string) => {
    try {
      await gatewayFetch("/api/v1/channels/unpair", { method: "POST", body: JSON.stringify({ channel, peerId }) });
      toast.success(zh ? "\u5DF2\u64A4\u9500" : "Revoked");
      fetchPairings();
    } catch (e: any) {
      toast.fromError(zh ? "\u64CD\u4F5C\u5931\u8D25" : "Action failed", e);
    }
  };

  const V2 = { bg2: "#141618", bg3: "#1a1c22", bd: "rgba(255,255,255,.055)", bd2: "rgba(255,255,255,.09)", t0: "#eceaf4", t1: "#9896a4", t2: "#4a4858", t3: "#2e2c3a", or: "#f97316", green: "#2dd4a0", gbrd: "rgba(45,212,160,.18)", glo: "rgba(45,212,160,.07)", red: "#d95f5f", rbrd: "rgba(217,95,95,.18)", rlo: "rgba(217,95,95,.08)", mono: "'JetBrains Mono', monospace" };

  const fmtTtl = (s: number) => { const m = Math.floor(s / 60); return m > 0 ? `${m}min` : `${s}s`; };

  return (
    <div style={{ flex: 1, overflow: "hidden", display: "flex", flexDirection: "column" }}>
      <div style={{ padding: "24px 28px 0", flexShrink: 0 }}>
        <div style={{ fontSize: 20, fontWeight: 700, color: V2.t0, letterSpacing: -0.4 }}>{zh ? "\u914D\u5BF9\u5BA1\u6279" : "Pairing Approval"}</div>
        <div style={{ fontSize: 11, color: V2.t3, fontFamily: V2.mono, marginTop: 3 }}>{zh ? "\u5BA1\u6279\u7528\u6237\u7684\u901A\u9053\u914D\u5BF9\u7533\u8BF7" : "Approve or reject user channel pairing requests"}</div>
      </div>

      <div style={{ padding: "20px 28px 28px", flex: 1, overflowY: "auto" }}>
        {/* Pending */}
        <div style={{ fontSize: 11, fontWeight: 600, color: V2.t1, letterSpacing: 0.4, textTransform: "uppercase", marginBottom: 10, display: "flex", alignItems: "center", gap: 8 }}>
          {zh ? "\u5F85\u5BA1\u6838" : "Pending"} <span style={{ fontSize: 9, padding: "1px 7px", borderRadius: 3, background: pending.length > 0 ? V2.or : V2.bg3, color: pending.length > 0 ? "#fff" : V2.t2 }}>{pending.length}</span>
        </div>

        {loading ? <div style={{ color: V2.t3, padding: 20, textAlign: "center" }}>...</div>
        : pending.length === 0 ? (
          <div style={{ background: V2.bg2, border: `1px solid ${V2.bd}`, borderRadius: 11, padding: "30px 0", textAlign: "center", color: V2.t3, marginBottom: 24 }}>
            {zh ? "\u6682\u65E0\u5F85\u5BA1\u6838\u7684\u914D\u5BF9\u8BF7\u6C42" : "No pending pairing requests"}
          </div>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 8, marginBottom: 24 }}>
            {pending.map((r, i) => (
              <div key={`${r.channel}-${r.peerId}-${i}`} style={{ background: V2.bg2, border: `1px solid ${V2.bd}`, borderRadius: 11, padding: "14px 16px", display: "flex", alignItems: "center", gap: 12 }}>
                <div style={{ width: 8, height: 8, borderRadius: "50%", background: V2.or, flexShrink: 0 }} />
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ fontSize: 13, fontWeight: 600, color: V2.t0, fontFamily: V2.mono }}>{r.code}</div>
                  <div style={{ fontSize: 10, color: V2.t3, fontFamily: V2.mono, marginTop: 2 }}>{r.channel} {"\u00B7"} {r.peerId.slice(0, 16)}... {"\u00B7"} {fmtTtl(r.ttlSeconds)}</div>
                </div>
                <button onClick={() => handleApprove(r.code)}
                  style={{ padding: "6px 14px", borderRadius: 7, border: `1px solid ${V2.gbrd}`, background: V2.glo, color: V2.green, fontSize: 11, fontWeight: 600, cursor: "pointer" }}>
                  {zh ? "\u901A\u8FC7" : "Approve"}
                </button>
              </div>
            ))}
          </div>
        )}

        {/* Approved */}
        {approved.length > 0 && (
          <>
            <div style={{ fontSize: 11, fontWeight: 600, color: V2.t1, letterSpacing: 0.4, textTransform: "uppercase", marginBottom: 10 }}>
              {zh ? "\u5DF2\u914D\u5BF9" : "Approved"} <span style={{ fontSize: 9, padding: "1px 7px", borderRadius: 3, background: V2.bg3, color: V2.t2 }}>{approved.length}</span>
            </div>
            <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
              {approved.map((r, i) => (
                <div key={`${r.channel}-${r.peerId}-${i}`} style={{ background: V2.bg2, border: `1px solid ${V2.bd}`, borderRadius: 9, padding: "10px 14px", display: "flex", alignItems: "center", gap: 10 }}>
                  <div style={{ width: 6, height: 6, borderRadius: "50%", background: V2.green, flexShrink: 0 }} />
                  <div style={{ flex: 1, fontSize: 11, color: V2.t1, fontFamily: V2.mono }}>{r.peerId.slice(0, 20)}... <span style={{ color: V2.t3 }}>{r.channel}</span></div>
                  <button onClick={() => handleRevoke(r.channel, r.peerId)}
                    style={{ padding: "4px 10px", borderRadius: 6, border: `1px solid ${V2.rbrd}`, background: V2.rlo, color: V2.red, fontSize: 10, fontWeight: 600, cursor: "pointer" }}>
                    {zh ? "\u64A4\u9500" : "Revoke"}
                  </button>
                </div>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}

function getTabFromLocation(search?: string): PanelPage {
  const qs = search || (typeof window !== "undefined" ? window.location.hash.split("?")[1] || "" : "");
  const params = new URLSearchParams(qs);
  const tab = params.get("tab");
  if (["config", "agents", "cron", "skills", "status", "workspace", "doctor", "pairing", "wizard"].includes(tab || "")) {
    return tab as PanelPage;
  }
  return "status";
}

export function RsClawPanel() {
  const navigate = useNavigate();
  const location = useLocation();
  const [activePage, setActivePage] = useState<PanelPage>(() => getTabFromLocation(location.search));
  const [gatewayRunning, setGatewayRunning] = useState(false);

  // Sync tab with URL changes (sidebar quick nav clicks)
  useEffect(() => {
    setActivePage(getTabFromLocation(location.search));
  }, [location.search]);

  useEffect(() => {
    (async () => {
      // Ensure gateway URL + auth token are set before any API call
      try {
        const tauriInvoke = (window as any).__TAURI__?.invoke;
        if (tauriInvoke) {
          const gw: any = await tauriInvoke("get_gateway_port");
          if (gw?.url) {
            setGatewayUrl(gw.url);
            if (gw.token) setAuthToken(gw.token);
          }
        }
      } catch {}
      try {
        await getHealth();
        setGatewayRunning(true);
      } catch {
        setGatewayRunning(false);
      }
    })();
  }, []);

  return (
    <ErrorBoundary>
      <div className={styles["rsclaw-panel-page"]}>
        {/* Header */}
        <div className={styles["panel-header"]}>
          <div className={styles["panel-title"]}>{Locale.RsClawPanel.Title}</div>
          <div className={styles["panel-header-right"]}>
            <button
              className={styles["tb-btn"]}
              onClick={() => navigate(Path.Home)}
            >
              {Locale.RsClawPanel.BackToChat}
            </button>
          </div>
        </div>

        {/* Body = nav + content */}
        <div className={styles["panel-body"]}>
          {/* Sub Navigation */}
          <div className={styles["rsp-nav"]}>
            <div className={styles["rsp-section"]}>{Locale.RsClawPanel.Nav.Status}</div>
            <button
              className={`${styles["rsp-item"]} ${activePage === "status" ? styles["active"] : ""}`}
              onClick={() => setActivePage("status")}
            >
              <span className={styles["rsp-item-icon"]}>&#x1F4E1;</span>
              {Locale.RsClawPanel.Nav.GatewayStatus}
            </button>

            <div className={styles["rsp-section"]}>{Locale.RsClawPanel.Nav.Config}</div>
            <button
              className={`${styles["rsp-item"]} ${activePage === "config" ? styles["active"] : ""}`}
              onClick={() => setActivePage("config")}
            >
              <span className={styles["rsp-item-icon"]}>&#x2699;&#xFE0F;</span>
              {Locale.RsClawPanel.Nav.ConfigEditor}
            </button>
            <button
              className={`${styles["rsp-item"]} ${activePage === "pairing" ? styles["active"] : ""}`}
              onClick={() => setActivePage("pairing")}
            >
              <span className={styles["rsp-item-icon"]}>&#x1F510;</span>
              {getLang() === "cn" ? "\u914D\u5BF9\u5BA1\u6279" : "Pairing Approval"}
            </button>

            <div className={styles["rsp-section"]}>{getLang() === "cn" ? "\u667A\u80FD\u4F53" : "Agents"}</div>
            <button
              className={`${styles["rsp-item"]} ${activePage === "agents" ? styles["active"] : ""}`}
              onClick={() => setActivePage("agents")}
            >
              <span className={styles["rsp-item-icon"]}>&#x1F916;</span>
              {Locale.RsClawPanel.Nav.AgentManager}
            </button>
            <button
              className={`${styles["rsp-item"]} ${activePage === "cron" ? styles["active"] : ""}`}
              onClick={() => setActivePage("cron")}
            >
              <span className={styles["rsp-item-icon"]}>&#x23F0;</span>
              {getLang() === "cn" ? "\u5B9A\u65F6\u4EFB\u52A1" : "Cron Tasks"}
            </button>

            <div className={styles["rsp-section"]}>{getLang() === "cn" ? "\u6269\u5C55" : "Extensions"}</div>
            <button
              className={`${styles["rsp-item"]} ${activePage === "skills" ? styles["active"] : ""}`}
              onClick={() => setActivePage("skills")}
            >
              <span className={styles["rsp-item-icon"]}>&#x1F527;</span>
              {getLang() === "cn" ? "\u6280\u80FD\u7BA1\u7406" : "Skills"}
            </button>

            <div className={styles["rsp-section"]}>{getLang() === "cn" ? "\u7CFB\u7EDF" : "System"}</div>
            <button
              className={`${styles["rsp-item"]} ${activePage === "doctor" ? styles["active"] : ""}`}
              onClick={() => setActivePage("doctor")}
            >
              <span className={styles["rsp-item-icon"]}>&#x1F6E1;&#xFE0F;</span>
              {Locale.RsClawPanel.Doctor.PageTitle}
            </button>
          </div>

          {/* Content */}
          <div className={styles["rsp-content"]}>
            {activePage === "status" && <StatusPage />}
            {activePage === "config" && <ErrorBoundary>{(window as any).__TAURI__?.invoke ? <TauriConfigPage /> : <ConfigEditorPage />}</ErrorBoundary>}
            {activePage === "agents" && <AgentManagerPage />}
            {activePage === "cron" && <CronTaskPage />}
            {activePage === "skills" && <SkillsPage />}
            {activePage === "workspace" && <WorkspacePage />}
            {activePage === "doctor" && <DoctorPage />}
            {activePage === "pairing" && <PairingPage />}
          </div>
        </div>
      </div>
    </ErrorBoundary>
  );
}
