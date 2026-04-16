import { useState, useEffect, useMemo, useCallback } from "react";

import styles from "./settings.module.scss";

import ResetIcon from "../icons/reload.svg";
import CloseIcon from "../icons/close.svg";
import ClearIcon from "../icons/clear.svg";
import LoadingIcon from "../icons/three-dots.svg";

import {
  List,
  ListItem,
  PasswordInput,
  Select,
  showConfirm,
} from "./ui-lib";

import { IconButton } from "./button";
import {
  SubmitKey,
  useChatStore,
  Theme,
  useUpdateStore,
  useAccessStore,
  useAppConfig,
} from "../store";

import Locale, {
  AllLangs,
  ALL_LANG_OPTIONS,
  changeLang,
  getLang,
} from "../locales";
import { copyToClipboard, clientUpdate, semverCompare } from "../utils";
import Link from "next/link";
import {
  OPENAI_BASE_URL,
  Path,
  RELEASE_URL,
  UPDATE_URL,
} from "../constant";
import { ErrorBoundary } from "./error";
import { InputRange } from "./input-range";
import { useNavigate } from "react-router-dom";
import { Avatar, AvatarPicker } from "./emoji";
import { getClientConfig } from "../config/client";
import { Popover } from "./ui-lib";
import { getAgents } from "../lib/rsclaw-api";

function AgentSelect(props: { value: string; onChange: (v: string) => void }) {
  const [agents, setAgents] = useState<{ id: string; name?: string; model?: string }[]>([]);
  const [loaded, setLoaded] = useState(false);

  useEffect(() => {
    getAgents()
      .then((data) => {
        const list = Array.isArray(data) ? data : data.agents || [];
        setAgents(list);
        setLoaded(true);
      })
      .catch(() => setLoaded(true));
  }, []);

  return (
    <select
      aria-label={Locale.RsClawSettings.Agent}
      value={props.value}
      onChange={(e) => props.onChange(e.target.value)}
    >
      {!loaded && (
        <option value={props.value}>{Locale.RsClawSettings.AgentLoading}</option>
      )}
      {loaded && agents.length === 0 && (
        <option value={props.value}>{props.value || Locale.RsClawSettings.AgentDefault}</option>
      )}
      {agents.map((a) => (
        <option key={a.id} value={a.id}>
          {a.name || a.id}{a.model ? ` (${a.model})` : ""}
        </option>
      ))}
    </select>
  );
}

function DangerItems() {
  const chatStore = useChatStore();
  const appConfig = useAppConfig();

  return (
    <List>
      <ListItem
        title={Locale.Settings.Danger.Reset.Title}
        subTitle={Locale.Settings.Danger.Reset.SubTitle}
      >
        <IconButton
          aria={Locale.Settings.Danger.Reset.Title}
          text={Locale.Settings.Danger.Reset.Action}
          onClick={async () => {
            if (await showConfirm(Locale.Settings.Danger.Reset.Confirm)) {
              appConfig.reset();
            }
          }}
          type="danger"
        />
      </ListItem>
      <ListItem
        title={Locale.Settings.Danger.Clear.Title}
        subTitle={Locale.Settings.Danger.Clear.SubTitle}
      >
        <IconButton
          aria={Locale.Settings.Danger.Clear.Title}
          text={Locale.Settings.Danger.Clear.Action}
          onClick={async () => {
            if (await showConfirm(Locale.Settings.Danger.Clear.Confirm)) {
              chatStore.clearAllData();
            }
          }}
          type="danger"
        />
      </ListItem>
    </List>
  );
}

export function Settings() {
  const navigate = useNavigate();
  const [showEmojiPicker, setShowEmojiPicker] = useState(false);
  const config = useAppConfig();
  const updateConfig = config.update;

  const updateStore = useUpdateStore();
  const [checkingUpdate, setCheckingUpdate] = useState(false);
  const currentVersion = updateStore.formatVersion(updateStore.version);
  const remoteId = updateStore.formatVersion(updateStore.remoteVersion);
  const hasNewVersion = semverCompare(currentVersion, remoteId) === -1;
  const updateUrl = getClientConfig()?.isApp ? RELEASE_URL : UPDATE_URL;

  function checkUpdate(force = false) {
    setCheckingUpdate(true);
    updateStore.getLatestVersion(force).then(() => {
      setCheckingUpdate(false);
    });
  }

  const accessStore = useAccessStore();

  // Auto-start at login (Tauri only)
  const [autoStart, setAutoStart] = useState(false);
  const [autoStartLoading, setAutoStartLoading] = useState(true);
  useEffect(() => {
    let cancelled = false;
    import("../utils/tauri").then(({ isTauri, invoke }) => {
      if (!isTauri || cancelled) { setAutoStartLoading(false); return; }
      invoke("get_auto_start").then((v: any) => {
        if (!cancelled) setAutoStart(!!v);
      }).catch(() => {}).finally(() => { if (!cancelled) setAutoStartLoading(false); });
    }).catch(() => setAutoStartLoading(false));
    return () => { cancelled = true; };
  }, []);
  const toggleAutoStart = useCallback(async () => {
    try {
      const { isTauri, invoke } = await import("../utils/tauri");
      if (!isTauri) return;
      const next = !autoStart;
      await invoke("set_auto_start", { enable: next });
      setAutoStart(next);
    } catch {}
  }, [autoStart]);

  useEffect(() => {
    checkUpdate();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const keydownEvent = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        navigate(Path.Home);
      }
    };
    document.addEventListener("keydown", keydownEvent);
    return () => {
      document.removeEventListener("keydown", keydownEvent);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const clientConfig = useMemo(() => getClientConfig(), []);

  return (
    <ErrorBoundary>
      <div className="window-header" data-tauri-drag-region>
        <div className="window-header-title">
          <div className="window-header-main-title">
            {Locale.Settings.Title}
          </div>
          <div className="window-header-sub-title">
            {Locale.Settings.SubTitle}
          </div>
        </div>
        <div className="window-actions">
          <div className="window-action-button"></div>
          <div className="window-action-button"></div>
          <div className="window-action-button">
            <IconButton
              aria={Locale.UI.Close}
              icon={<CloseIcon />}
              onClick={() => navigate(Path.Home)}
              bordered
            />
          </div>
        </div>
      </div>
      <div className={styles["settings"]}>
        <List>
          <ListItem title={Locale.Settings.Avatar}>
            <Popover
              onClose={() => setShowEmojiPicker(false)}
              content={
                <AvatarPicker
                  onEmojiClick={(avatar: string) => {
                    updateConfig((config) => (config.avatar = avatar));
                    setShowEmojiPicker(false);
                  }}
                />
              }
              open={showEmojiPicker}
            >
              <div
                aria-label={Locale.Settings.Avatar}
                tabIndex={0}
                className={styles.avatar}
                onClick={() => {
                  setShowEmojiPicker(!showEmojiPicker);
                }}
              >
                <Avatar avatar={config.avatar} />
              </div>
            </Popover>
          </ListItem>

          <ListItem
            title={Locale.Settings.Update.Version(currentVersion ?? "unknown")}
            subTitle={
              checkingUpdate
                ? Locale.Settings.Update.IsChecking
                : hasNewVersion
                ? Locale.Settings.Update.FoundUpdate(remoteId ?? "ERROR")
                : Locale.Settings.Update.IsLatest
            }
          >
            {checkingUpdate ? (
              <LoadingIcon />
            ) : hasNewVersion ? (
              clientConfig?.isApp ? (
                <IconButton
                  icon={<ResetIcon></ResetIcon>}
                  text={Locale.Settings.Update.GoToUpdate}
                  onClick={() => clientUpdate()}
                />
              ) : (
                <Link href={updateUrl} target="_blank" className="link">
                  {Locale.Settings.Update.GoToUpdate}
                </Link>
              )
            ) : (
              <IconButton
                icon={<ResetIcon></ResetIcon>}
                text={Locale.Settings.Update.CheckUpdate}
                onClick={() => checkUpdate(true)}
              />
            )}
          </ListItem>

          {!autoStartLoading && (
            <ListItem
              title={Locale.RsClawSettings.AutoStart}
              subTitle={Locale.RsClawSettings.AutoStartSub}
            >
              <input
                type="checkbox"
                checked={autoStart}
                onChange={toggleAutoStart}
              />
            </ListItem>
          )}
        </List>

        <List>
          <ListItem
            title={Locale.RsClawSettings.GatewayUrl}
            subTitle={Locale.RsClawSettings.GatewayUrlSub}
          >
            <input
              aria-label={Locale.RsClawSettings.GatewayUrl}
              type="text"
              value={accessStore.openaiUrl}
              placeholder={OPENAI_BASE_URL}
              readOnly={process.env.NODE_ENV === "production"}
              style={process.env.NODE_ENV === "production" ? { opacity: 0.6, cursor: "not-allowed" } : undefined}
              onChange={(e) =>
                accessStore.update(
                  (access) => (access.openaiUrl = e.currentTarget.value),
                )
              }
            ></input>
          </ListItem>
          <ListItem
            title={Locale.RsClawSettings.Agent}
            subTitle={Locale.RsClawSettings.AgentSub}
          >
            <AgentSelect
              value={config.modelConfig.model as string}
              onChange={(v) =>
                config.update(
                  (config) => (config.modelConfig.model = v as any),
                )
              }
            />
          </ListItem>
        </List>

        <List>
          <ListItem title={Locale.Settings.SendKey}>
            <Select
              aria-label={Locale.Settings.SendKey}
              value={config.submitKey}
              onChange={(e) => {
                updateConfig(
                  (config) =>
                    (config.submitKey = e.target.value as any as SubmitKey),
                );
              }}
            >
              {Object.values(SubmitKey).map((v) => (
                <option value={v} key={v}>
                  {v}
                </option>
              ))}
            </Select>
          </ListItem>

          {/* Theme switcher hidden: panel only supports dark mode */}

          <ListItem title={Locale.Settings.Lang.Name}>
            <Select
              aria-label={Locale.Settings.Lang.Name}
              value={getLang()}
              onChange={(e) => {
                changeLang(e.target.value as any);
              }}
            >
              {AllLangs.map((lang) => (
                <option value={lang} key={lang}>
                  {ALL_LANG_OPTIONS[lang]}
                </option>
              ))}
            </Select>
          </ListItem>

          <ListItem
            title={Locale.Settings.FontSize.Title}
            subTitle={Locale.Settings.FontSize.SubTitle}
          >
            <InputRange
              aria={Locale.Settings.FontSize.Title}
              title={`${config.fontSize ?? 14}px`}
              value={config.fontSize}
              min="12"
              max="40"
              step="1"
              onChange={(e) =>
                updateConfig(
                  (config) =>
                    (config.fontSize = Number.parseInt(e.currentTarget.value)),
                )
              }
            ></InputRange>
          </ListItem>
        </List>

        {/* Re-run setup wizard */}
        <List>
          <ListItem
            title={getLang() === "cn" ? "重新运行向导" : "Re-run Setup Wizard"}
            subTitle={getLang() === "cn" ? "重新进入首次启动引导流程" : "Re-enter the first-time setup wizard"}
          >
            <IconButton
              text={getLang() === "cn" ? "运行" : "Run"}
              onClick={() => {
                const msg = getLang() === "cn" ? "\u786E\u8BA4\u91CD\u65B0\u8FD0\u884C\u5411\u5BFC\uFF1F\u5F53\u524D\u914D\u7F6E\u4E0D\u4F1A\u88AB\u5220\u9664\u3002" : "Re-run setup wizard? Current config will not be deleted.";
                if (!window.confirm(msg)) return;
                const { resetSetup } = require("../lib/first-launch");
                resetSetup();
                window.location.hash = "#/onboarding";
                window.location.reload();
              }}
            />
          </ListItem>
        </List>
      </div>
    </ErrorBoundary>
  );
}
