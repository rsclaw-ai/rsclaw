import React, { Fragment, useEffect, useMemo, useRef, useState } from "react";

import styles from "./home.module.scss";

import { IconButton } from "./button";
import SettingsIcon from "../icons/settings.svg";
import ChatGptIcon from "../icons/chatgpt.svg";
import AddIcon from "../icons/add.svg";
import DeleteIcon from "../icons/delete.svg";
import DragIcon from "../icons/drag.svg";
import LightningIcon from "../icons/lightning.svg";

import Locale, { getLang } from "../locales";

import { useAppConfig, useChatStore } from "../store";

import {
  DEFAULT_SIDEBAR_WIDTH,
  MAX_SIDEBAR_WIDTH,
  MIN_SIDEBAR_WIDTH,
  NARROW_SIDEBAR_WIDTH,
  Path,
} from "../constant";

import { Link, useNavigate } from "react-router-dom";
import { isIOS, useMobileScreen } from "../utils";
import dynamic from "next/dynamic";
import { showConfirm } from "./ui-lib";
import clsx from "clsx";
import { getAgents, getHealth } from "../lib/rsclaw-api";

const ChatList = dynamic(async () => (await import("./chat-list")).ChatList, {
  loading: () => null,
});

function NewChatDialog(props: {
  onClose: () => void;
  onCreate: (topic: string, agentId: string) => void;
}) {
  const [topic, setTopic] = useState("");
  const [agentId, setAgentId] = useState("");
  const [agents, setAgents] = useState<{ id: string; name?: string; model?: string }[]>([]);

  useEffect(() => {
    getAgents()
      .then((data) => {
        const list = Array.isArray(data) ? data : data.agents || [];
        setAgents(list);
        if (list.length > 0 && !agentId) setAgentId(list[0].id);
      })
      .catch(() => {});
  }, []);

  return (
    <div className={styles["new-chat-overlay"]} onClick={props.onClose}>
      <div
        className={styles["new-chat-dialog"]}
        onClick={(e) => e.stopPropagation()}
      >
        <div className={styles["new-chat-title"]}>
          {Locale.NewChatDialog.Title}
        </div>
        <div className={styles["new-chat-field"]}>
          <label>{Locale.NewChatDialog.SessionName}</label>
          <input
            autoFocus
            value={topic}
            placeholder={Locale.NewChatDialog.SessionNamePlaceholder}
            onChange={(e) => setTopic(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") props.onCreate(topic, agentId);
            }}
          />
        </div>
        {agents.length > 0 && (
          <div className={styles["new-chat-field"]}>
            <label>{Locale.NewChatDialog.Agent}</label>
            <select
              value={agentId}
              onChange={(e) => setAgentId(e.target.value)}
            >
              {agents.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.name || a.id}
                  {a.model ? ` (${a.model})` : ""}
                </option>
              ))}
            </select>
          </div>
        )}
        <div className={styles["new-chat-actions"]}>
          <button
            className={styles["new-chat-btn"]}
            onClick={props.onClose}
          >
            {Locale.NewChatDialog.Cancel}
          </button>
          <button
            className={styles["new-chat-btn-primary"]}
            onClick={() => props.onCreate(topic, agentId)}
          >
            {Locale.NewChatDialog.Create}
          </button>
        </div>
      </div>
    </div>
  );
}

// Track whether user manually stopped the gateway
export function setUserStopped(v: boolean) { try { localStorage.setItem("rsclaw-user-stopped", v ? "1" : ""); } catch {} }
function getUserStopped() { try { return localStorage.getItem("rsclaw-user-stopped") === "1"; } catch { return false; } }

function GatewayStatus({ narrow }: { narrow: boolean }) {
  const [status, setStatus] = React.useState<"online" | "offline" | "checking" | "starting" | "failed">("checking");
  const [confirmAction, setConfirmAction] = React.useState<"start"|"restart"|"stop"|null>(null);
  const [starting, setStarting] = React.useState(false);
  const [errorMsg, setErrorMsg] = React.useState("");
  const failCount = React.useRef(0);
  const autoStarted = React.useRef(false);
  const navigate = useNavigate();
  const zh = getLang() === "cn";

  const doStart = async () => {
    setStarting(true);
    setStatus("starting");
    setErrorMsg("");
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) await tauriInvoke("start_gateway");
      setUserStopped(false);
      setTimeout(() => {
        getHealth()
          .then(() => { setStatus("online"); setStarting(false); failCount.current = 0; setErrorMsg(""); })
          .catch(() => {
            failCount.current++;
            setStarting(false);
            setStatus("failed");
            if (failCount.current >= 2) {
              setErrorMsg(zh ? "网关启动失败，请检查端口是否被占用或配置是否正确" : "Gateway failed to start. Check port conflicts or config errors.");
            }
          });
      }, 1000);
    } catch (e: any) {
      failCount.current++;
      setStarting(false);
      setStatus("failed");
      setErrorMsg(String(e?.message || e || ""));
    }
  };

  const doStop = async () => {
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) await tauriInvoke("stop_gateway");
      setUserStopped(true);
      setStatus("offline");
      setErrorMsg("");
      failCount.current = 0;
    } catch {}
  };

  const doRestart = async () => {
    setStarting(true);
    setStatus("starting");
    setErrorMsg("");
    try {
      const tauriInvoke = (window as any).__TAURI__?.invoke;
      if (tauriInvoke) {
        await tauriInvoke("stop_gateway");
        await new Promise((r) => setTimeout(r, 500));
        await tauriInvoke("start_gateway");
      }
      setTimeout(() => {
        getHealth()
          .then(() => { setStatus("online"); setStarting(false); failCount.current = 0; setErrorMsg(""); })
          .catch(() => {
            failCount.current++;
            setStarting(false);
            setStatus("failed");
          });
      }, 1000);
    } catch {
      setStarting(false);
      setStatus("failed");
    }
  };

  const doDiagnose = async () => {
    navigate(Path.RsClawPanel + "?tab=doctor");
  };

  const executeConfirm = () => {
    const action = confirmAction;
    setConfirmAction(null);
    if (action === "start") doStart();
    else if (action === "restart") doRestart();
    else if (action === "stop") doStop();
  };

  React.useEffect(() => {
    const check = () => {
      getHealth()
        .then(() => { setStatus("online"); autoStarted.current = false; })
        .catch(() => {
          if (starting) return; // don't overwrite "starting" state
          setStatus("offline");
          // Auto-start on first offline detection (unless user manually stopped)
          const tauriInvoke = (window as any).__TAURI__?.invoke;
          if (tauriInvoke && !getUserStopped() && !autoStarted.current) {
            autoStarted.current = true;
            doStart();
          }
        });
    };
    check();
    const timer = setInterval(check, 10000);
    return () => clearInterval(timer);
  }, []);

  const isOnline = status === "online";
  const isFailed = status === "failed";
  const isChecking = status === "checking";
  const isStarting = status === "starting" || starting;
  const color = isOnline ? "#2dd4a0" : (isStarting || isChecking) ? "#f5a623" : isFailed ? "#d95f5f" : "#d95f5f";
  const label = isOnline ? Locale.RsClawPanel.Running
    : isStarting ? (zh ? "\u542F\u52A8\u4E2D..." : "Starting...")
    : isChecking ? (zh ? "\u68C0\u67E5\u4E2D..." : "Checking...")
    : isFailed ? (zh ? "\u542F\u52A8\u5931\u8D25" : "Start Failed")
    : Locale.RsClawPanel.Offline;

  return (
    <div
      className={styles["gateway-status"]}
      onClick={isOnline ? () => navigate(Path.RsClawPanel + "?tab=status") : undefined}
      style={{ cursor: isOnline ? "pointer" : "default", position: "relative" }}
    >
      <span className={styles["gateway-dot"]} style={{ background: color }} />
      {!narrow && (
        <>
          <span className={styles["gateway-label"]} style={{ flex: 1 }}>{label}</span>
          {!isStarting && !isChecking && (
            <div style={{ display: "flex", gap: 4, marginLeft: "auto" }} onClick={(e) => e.stopPropagation()}>
              {isFailed ? (
                <>
                  <button
                    onClick={() => doStart()}
                    style={{
                      padding: "2px 8px", borderRadius: 5, fontSize: 11, fontFamily: "inherit",
                      border: "1px solid rgba(249,115,22,.25)", background: "transparent",
                      color: "rgba(249,115,22,.8)", cursor: "pointer", transition: "color .12s",
                    }}
                    onMouseEnter={(e) => (e.currentTarget.style.color = "rgba(249,115,22,1)")}
                    onMouseLeave={(e) => (e.currentTarget.style.color = "rgba(249,115,22,.8)")}
                  >
                    {zh ? "\u91CD\u8BD5" : "Retry"}
                  </button>
                  <button
                    onClick={doDiagnose}
                    style={{
                      padding: "2px 8px", borderRadius: 5, fontSize: 11, fontFamily: "inherit",
                      border: "1px solid rgba(255,255,255,.12)", background: "transparent",
                      color: "rgba(255,255,255,.4)", cursor: "pointer", transition: "color .12s",
                    }}
                    onMouseEnter={(e) => (e.currentTarget.style.color = "rgba(255,255,255,.65)")}
                    onMouseLeave={(e) => (e.currentTarget.style.color = "rgba(255,255,255,.4)")}
                  >
                    {zh ? "\u8BCA\u65AD" : "Diagnose"}
                  </button>
                </>
              ) : (
                <>
                  <button
                    onClick={() => setConfirmAction("restart")}
                    style={{
                      padding: "2px 8px", borderRadius: 5, fontSize: 11, fontFamily: "inherit",
                      border: "1px solid rgba(255,255,255,.12)", background: "transparent",
                      color: "rgba(255,255,255,.4)", cursor: "pointer", transition: "color .12s",
                    }}
                    onMouseEnter={(e) => (e.currentTarget.style.color = "rgba(255,255,255,.65)")}
                    onMouseLeave={(e) => (e.currentTarget.style.color = "rgba(255,255,255,.4)")}
                  >
                    {zh ? "\u91CD\u542F" : "Restart"}
                  </button>
                  {isOnline ? (
                    <button
                      onClick={() => setConfirmAction("stop")}
                      style={{
                        padding: "2px 8px", borderRadius: 5, fontSize: 11, fontFamily: "inherit",
                        border: "1px solid rgba(217,95,95,.2)", background: "transparent",
                        color: "rgba(217,95,95,.7)", cursor: "pointer", transition: "color .12s",
                      }}
                      onMouseEnter={(e) => (e.currentTarget.style.color = "rgba(217,95,95,.9)")}
                      onMouseLeave={(e) => (e.currentTarget.style.color = "rgba(217,95,95,.7)")}
                    >
                      {zh ? "\u505C\u6B62" : "Stop"}
                    </button>
                  ) : (
                    <button
                      onClick={() => setConfirmAction("start")}
                      style={{
                        padding: "2px 8px", borderRadius: 5, fontSize: 11, fontFamily: "inherit",
                        border: "1px solid rgba(249,115,22,.25)", background: "transparent",
                        color: "rgba(249,115,22,.8)", cursor: "pointer", transition: "color .12s",
                      }}
                      onMouseEnter={(e) => (e.currentTarget.style.color = "rgba(249,115,22,1)")}
                      onMouseLeave={(e) => (e.currentTarget.style.color = "rgba(249,115,22,.8)")}
                    >
                      {zh ? "\u542F\u52A8" : "Start"}
                    </button>
                  )}
                </>
              )}
            </div>
          )}
          {isStarting && (
            <span style={{ marginLeft: "auto", fontSize: 10, color: "#f97316" }}>...</span>
          )}
        </>
      )}
      {/* Error message after 2+ failures */}
      {!narrow && isFailed && errorMsg && (
        <div style={{ fontSize: 9, color: "#d95f5f", padding: "2px 8px 0", lineHeight: 1.3, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
          {errorMsg}
        </div>
      )}
      {/* Confirm popover */}
      {confirmAction && (
        <div onClick={(e) => e.stopPropagation()} style={{
          position: "absolute", bottom: "100%", left: 0, right: 0,
          marginBottom: 6, padding: "10px 12px",
          background: "var(--white)", border: "1px solid var(--border-in-light)",
          borderRadius: 8, boxShadow: "0 4px 12px rgba(0,0,0,0.3)",
          zIndex: 100,
        }}>
          <div style={{ fontSize: 11, color: "var(--black)", marginBottom: 8 }}>
            {confirmAction === "stop" ? (zh ? "\u786E\u8BA4\u505C\u6B62\u7F51\u5173\uFF1F" : "Stop gateway?")
              : confirmAction === "restart" ? (zh ? "\u786E\u8BA4\u91CD\u542F\u7F51\u5173\uFF1F" : "Restart gateway?")
              : (zh ? "\u786E\u8BA4\u542F\u52A8\u7F51\u5173\uFF1F" : "Start gateway?")}
          </div>
          <div style={{ display: "flex", gap: 6, justifyContent: "flex-end" }}>
            <button onClick={() => setConfirmAction(null)}
              style={{ fontSize: 10, padding: "3px 10px", borderRadius: 5, border: "1px solid var(--border-in-light)", background: "transparent", color: "var(--black)", cursor: "pointer" }}>
              {zh ? "\u53D6\u6D88" : "Cancel"}
            </button>
            <button onClick={executeConfirm}
              style={{
                fontSize: 10, padding: "3px 10px", borderRadius: 5, border: "none", cursor: "pointer", fontWeight: 600,
                background: confirmAction === "stop" ? "#d95f5f" : "#f97316", color: "#fff",
              }}>
              {confirmAction === "stop" ? (zh ? "\u505C\u6B62" : "Stop")
                : confirmAction === "restart" ? (zh ? "\u91CD\u542F" : "Restart")
                : (zh ? "\u542F\u52A8" : "Start")}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

export function useHotKey() {
  const chatStore = useChatStore();

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.altKey || e.ctrlKey) {
        if (e.key === "ArrowUp") {
          chatStore.nextSession(-1);
        } else if (e.key === "ArrowDown") {
          chatStore.nextSession(1);
        }
      }
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });
}

export function useDragSideBar() {
  const limit = (x: number) => Math.min(MAX_SIDEBAR_WIDTH, x);

  const config = useAppConfig();
  const startX = useRef(0);
  const startDragWidth = useRef(config.sidebarWidth ?? DEFAULT_SIDEBAR_WIDTH);
  const lastUpdateTime = useRef(Date.now());

  const toggleSideBar = () => {
    config.update((config) => {
      if (config.sidebarWidth < MIN_SIDEBAR_WIDTH) {
        config.sidebarWidth = DEFAULT_SIDEBAR_WIDTH;
      } else {
        config.sidebarWidth = NARROW_SIDEBAR_WIDTH;
      }
    });
  };

  const onDragStart = (e: MouseEvent) => {
    // Remembers the initial width each time the mouse is pressed
    startX.current = e.clientX;
    startDragWidth.current = config.sidebarWidth;
    const dragStartTime = Date.now();

    const handleDragMove = (e: MouseEvent) => {
      if (Date.now() < lastUpdateTime.current + 20) {
        return;
      }
      lastUpdateTime.current = Date.now();
      const d = e.clientX - startX.current;
      const nextWidth = limit(startDragWidth.current + d);
      config.update((config) => {
        if (nextWidth < MIN_SIDEBAR_WIDTH) {
          config.sidebarWidth = NARROW_SIDEBAR_WIDTH;
        } else {
          config.sidebarWidth = nextWidth;
        }
      });
    };

    const handleDragEnd = () => {
      // In useRef the data is non-responsive, so `config.sidebarWidth` can't get the dynamic sidebarWidth
      window.removeEventListener("pointermove", handleDragMove);
      window.removeEventListener("pointerup", handleDragEnd);

      // if user click the drag icon, should toggle the sidebar
      const shouldFireClick = Date.now() - dragStartTime < 300;
      if (shouldFireClick) {
        toggleSideBar();
      }
    };

    window.addEventListener("pointermove", handleDragMove);
    window.addEventListener("pointerup", handleDragEnd);
  };

  const isMobileScreen = useMobileScreen();
  const shouldNarrow =
    !isMobileScreen && config.sidebarWidth < MIN_SIDEBAR_WIDTH;

  useEffect(() => {
    const barWidth = shouldNarrow
      ? NARROW_SIDEBAR_WIDTH
      : limit(config.sidebarWidth ?? DEFAULT_SIDEBAR_WIDTH);
    const sideBarWidth = isMobileScreen ? "100vw" : `${barWidth}px`;
    document.documentElement.style.setProperty("--sidebar-width", sideBarWidth);
  }, [config.sidebarWidth, isMobileScreen, shouldNarrow]);

  return {
    onDragStart,
    shouldNarrow,
  };
}

export function SideBarContainer(props: {
  children: React.ReactNode;
  onDragStart: (e: MouseEvent) => void;
  shouldNarrow: boolean;
  className?: string;
}) {
  const isMobileScreen = useMobileScreen();
  const isIOSMobile = useMemo(
    () => isIOS() && isMobileScreen,
    [isMobileScreen],
  );
  const { children, className, onDragStart, shouldNarrow } = props;
  return (
    <div
      className={clsx(styles.sidebar, className, {
        [styles["narrow-sidebar"]]: shouldNarrow,
      })}
      style={{
        // #3016 disable transition on ios mobile screen
        transition: isMobileScreen && isIOSMobile ? "none" : undefined,
      }}
    >
      {children}
      <div
        className={styles["sidebar-drag"]}
        onPointerDown={(e) => onDragStart(e as any)}
      >
        <DragIcon />
      </div>
    </div>
  );
}

export function SideBarHeader(props: {
  title?: string | React.ReactNode;
  subTitle?: string | React.ReactNode;
  logo?: React.ReactNode;
  children?: React.ReactNode;
  shouldNarrow?: boolean;
}) {
  const { title, subTitle, logo, children, shouldNarrow } = props;
  return (
    <Fragment>
      <div
        className={clsx(styles["sidebar-header"], {
          [styles["sidebar-header-narrow"]]: shouldNarrow,
        })}
        data-tauri-drag-region
      >
        <div className={styles["sidebar-title-container"]}>
          <div className={styles["sidebar-title"]} data-tauri-drag-region>
            {title}
          </div>
          <div className={styles["sidebar-sub-title"]}>{subTitle}</div>
        </div>
        <div className={clsx(styles["sidebar-logo"], "no-dark")}>{logo}</div>
      </div>
      {children}
    </Fragment>
  );
}

export function SideBarBody(props: {
  children: React.ReactNode;
  onClick?: (e: React.MouseEvent<HTMLDivElement, MouseEvent>) => void;
}) {
  const { onClick, children } = props;
  return (
    <div className={styles["sidebar-body"]} onClick={onClick}>
      {children}
    </div>
  );
}

export function SideBarTail(props: {
  primaryAction?: React.ReactNode;
  secondaryAction?: React.ReactNode;
}) {
  const { primaryAction, secondaryAction } = props;

  return (
    <div className={styles["sidebar-tail"]}>
      <div className={styles["sidebar-actions"]}>{primaryAction}</div>
      <div className={styles["sidebar-actions"]}>{secondaryAction}</div>
    </div>
  );
}

export function SideBar(props: { className?: string }) {
  useHotKey();
  const { onDragStart, shouldNarrow } = useDragSideBar();
  const navigate = useNavigate();
  const config = useAppConfig();
  const chatStore = useChatStore();
  const [showNewChat, setShowNewChat] = useState(false);

  return (
    <SideBarContainer
      onDragStart={onDragStart}
      shouldNarrow={shouldNarrow}
      {...props}
    >
      <SideBarHeader
        title={
          shouldNarrow ? (
            <img src="/rsclaw-icon.svg" alt="Rs" style={{ height: "28px", borderRadius: "6px" }} />
          ) : (
            <div style={{ display: "flex", alignItems: "center", gap: "10px" }}>
              <img src="/rsclaw-icon.svg" alt="" style={{ height: "32px", borderRadius: "7px" }} />
              <div>
                <div style={{ fontSize: "16px", fontWeight: 700, color: "#f0eff2", lineHeight: 1.2 }}>RsClaw</div>
                <div style={{ fontSize: "10px", color: "#6b6877", fontFamily: "'JetBrains Mono', monospace", letterSpacing: "0.5px" }}>
                  {Locale.RsClawPanel.SubTitle.toUpperCase()}
                </div>
              </div>
            </div>
          )
        }
        shouldNarrow={shouldNarrow}
      >
        <GatewayStatus narrow={shouldNarrow} />
        {!shouldNarrow && (
          <div className={styles["sidebar-quick-nav"]}>
            {[
              { tab: "status", icon: "\uD83D\uDCE1", label: Locale.RsClawPanel.Sidebar.Service },
              { tab: "config", icon: "\u2699\uFE0F", label: Locale.RsClawPanel.Sidebar.Config },
              { tab: "agents", icon: "\uD83E\uDD16", label: Locale.RsClawPanel.Sidebar.Agents },
              { tab: "pairing", icon: "\uD83D\uDD10", label: getLang() === "cn" ? "\u914D\u5BF9\u5BA1\u6279" : "Pairing" },
              { tab: "cron", icon: "\u23F0", label: getLang() === "cn" ? "\u5B9A\u65F6\u4EFB\u52A1" : "Cron" },
              { tab: "skills", icon: "\uD83D\uDD27", label: getLang() === "cn" ? "\u6280\u80FD\u7BA1\u7406" : "Skills" },
              { tab: "doctor", icon: "\uD83D\uDEE1\uFE0F", label: getLang() === "cn" ? "\u5B89\u5168\u68C0\u67E5" : "Doctor" },
            ].map((item) => (
              <button
                key={item.tab}
                className={styles["sidebar-quick-btn"]}
                onClick={() => navigate(Path.RsClawPanel + "?tab=" + item.tab)}
                title={item.label}
              >
                <span>{item.icon}</span>
                <span>{item.label}</span>
              </button>
            ))}
          </div>
        )}
      </SideBarHeader>
      <SideBarBody
        onClick={(e) => {
          if (e.target === e.currentTarget) {
            navigate(Path.Home);
          }
        }}
      >
        <ChatList narrow={shouldNarrow} />
      </SideBarBody>
      <SideBarTail
        primaryAction={
          <>
            <div className={clsx(styles["sidebar-action"], styles.mobile)}>
              <IconButton
                icon={<DeleteIcon />}
                onClick={async () => {
                  if (await showConfirm(Locale.Home.DeleteChat)) {
                    chatStore.deleteSession(chatStore.currentSessionIndex);
                  }
                }}
              />
            </div>
            <div className={styles["sidebar-action"]}>
              <Link to={Path.Settings}>
                <IconButton
                  aria={Locale.Settings.Title}
                  icon={<SettingsIcon />}
                  shadow
                />
              </Link>
            </div>
          </>
        }
        secondaryAction={
          <IconButton
            icon={<AddIcon />}
            text={shouldNarrow ? undefined : Locale.Home.NewChat}
            onClick={() => setShowNewChat(true)}
            shadow
          />
        }
      />
      {showNewChat && (
        <NewChatDialog
          onClose={() => setShowNewChat(false)}
          onCreate={(topic, agentId) => {
            chatStore.newSession(undefined, {
              topic: topic || undefined,
              agentId: agentId || undefined,
            });
            setShowNewChat(false);
            navigate(Path.Chat);
          }}
        />
      )}
    </SideBarContainer>
  );
}
