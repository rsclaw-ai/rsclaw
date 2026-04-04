"use client";

require("../polyfill");

import { useEffect, useState } from "react";
import styles from "./home.module.scss";

import BotIcon from "../icons/bot.svg";
import LoadingIcon from "../icons/three-dots.svg";

import { getCSSVar, useMobileScreen } from "../utils";
import Locale from "../locales";

import dynamic from "next/dynamic";
import { Path, SlotID } from "../constant";
import { ErrorBoundary } from "./error";
import { ToastContainer } from "./toast-container";
import { isFirstLaunch } from "../lib/first-launch";
import { setGatewayUrl, setAuthToken } from "../lib/rsclaw-api";

import { getISOLang, getLang } from "../locales";

import {
  HashRouter as Router,
  Route,
  Routes,
  useLocation,
  useNavigate,
} from "react-router-dom";
import { SideBar } from "./sidebar";
import { useAppConfig } from "../store/config";
import { AuthPage } from "./auth";
import { getClientConfig } from "../config/client";
import { type ClientApi, getClientApi } from "../client/api";
import { useAccessStore } from "../store";
import clsx from "clsx";

export function Loading(props: { noLogo?: boolean }) {
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);

  return (
    <div className={clsx("no-dark", styles["loading-content"])}>
      {!props.noLogo && (
        <img
          src="/rsclaw-icon.svg"
          alt="RsClaw"
          style={{ width: "72px", height: "72px", borderRadius: "16px" }}
        />
      )}
      <LoadingIcon />
      {!props.noLogo && mounted && (
        <div className={styles["loading-text"]}>
          {Locale.RsClawPanel.Splash}
        </div>
      )}
    </div>
  );
}

const Settings = dynamic(async () => (await import("./settings")).Settings, {
  loading: () => <Loading noLogo />,
});

const Chat = dynamic(async () => (await import("./chat")).Chat, {
  loading: () => <Loading noLogo />,
});

const SearchChat = dynamic(
  async () => (await import("./search-chat")).SearchChatPage,
  {
    loading: () => <Loading noLogo />,
  },
);

const GatewayControlPage = dynamic(
  async () => (await import("./gateway-control")).GatewayControlPage,
  {
    loading: () => <Loading noLogo />,
  },
);

const RsClawPanel = dynamic(
  async () => (await import("./rsclaw-panel")).RsClawPanel,
  {
    loading: () => <Loading noLogo />,
  },
);

const SetupWizardPage = dynamic(
  async () => (await import("./setup-wizard")).SetupWizardPage,
  {
    loading: () => <Loading noLogo />,
  },
);

const OnboardingPage = dynamic(
  async () => (await import("./onboarding")).OnboardingPage,
  {
    loading: () => <Loading />,
  },
);

const AgentManagerPage = dynamic(
  async () => (await import("./agent-manager")).AgentManagerPage,
  {
    loading: () => <Loading noLogo />,
  },
);

const ChannelConfigPage = dynamic(
  async () => (await import("./channel-config")).ChannelConfigPage,
  {
    loading: () => <Loading noLogo />,
  },
);

const CronManagerPage = dynamic(
  async () => (await import("./cron-manager")).CronManagerPage,
  {
    loading: () => <Loading noLogo />,
  },
);

export function useSwitchTheme() {
  const config = useAppConfig();

  useEffect(() => {
    // Force dark theme -- no light/auto support.
    document.body.classList.remove("light");
    document.body.classList.add("dark");

    const metaDescriptionDark = document.querySelector(
      'meta[name="theme-color"][media*="dark"]',
    );
    const metaDescriptionLight = document.querySelector(
      'meta[name="theme-color"][media*="light"]',
    );
    const themeColor = getCSSVar("--theme-color");
    metaDescriptionDark?.setAttribute("content", themeColor);
    metaDescriptionLight?.setAttribute("content", themeColor);
  }, []);
}

function useHtmlLang() {
  useEffect(() => {
    const lang = getISOLang();
    const htmlLang = document.documentElement.lang;

    if (lang !== htmlLang) {
      document.documentElement.lang = lang;
    }
  }, []);
}

const useHasHydrated = () => {
  const [hasHydrated, setHasHydrated] = useState<boolean>(false);

  useEffect(() => {
    setHasHydrated(true);
  }, []);

  return hasHydrated;
};

const loadAsyncGoogleFont = () => {
  const linkEl = document.createElement("link");
  const proxyFontUrl = "/google-fonts";
  const remoteFontUrl = "https://fonts.googleapis.com";
  const googleFontUrl =
    getClientConfig()?.buildMode === "export" ? remoteFontUrl : proxyFontUrl;
  linkEl.rel = "stylesheet";
  linkEl.href =
    googleFontUrl +
    "/css2?family=" +
    encodeURIComponent("Noto Sans:wght@300;400;700;900") +
    "&display=swap";
  document.head.appendChild(linkEl);
};

export function WindowContent(props: { children: React.ReactNode }) {
  return (
    <div className={styles["window-content"]} id={SlotID.AppBody}>
      {props?.children}
    </div>
  );
}

function Screen() {
  const config = useAppConfig();
  const location = useLocation();
  const isHome = location.pathname === Path.Home;
  const isAuth = location.pathname === Path.Auth;
  const isMobileScreen = useMobileScreen();
  const shouldTightBorder =
    getClientConfig()?.isApp || (config.tightBorder && !isMobileScreen);

  const navigate = useNavigate();

  useEffect(() => {
    // One-time migration: clear stale localStorage from older versions
    const STORAGE_VERSION = "rsclaw-store-v2";
    if (!localStorage.getItem(STORAGE_VERSION)) {
      localStorage.clear();
      localStorage.setItem(STORAGE_VERSION, "1");
    }

    loadAsyncGoogleFont();
    // Read gateway port from config via Tauri and sync everywhere
    (async () => {
      try {
        const tauriInvoke = (window as any).__TAURI__?.invoke;
        if (tauriInvoke) {
          const gw: any = await tauriInvoke("get_gateway_port");
          if (gw?.url) {
            setGatewayUrl(gw.url);
            if (gw.token) {
              setAuthToken(gw.token);
              try { localStorage.setItem("rsclaw-auth-token", gw.token); } catch {}
            }
            // Sync to access store so chat requests use correct gateway URL + token
            try {
              useAccessStore.getState().update((a) => {
                (a as any).openaiUrl = gw.url;
                if (gw.token) (a as any).openaiApiKey = gw.token;
              });
            } catch {}
          }
        }
      } catch {}
    })();
    // First launch: redirect to onboarding
    if (isFirstLaunch() && location.pathname !== Path.Onboarding) {
      navigate(Path.Onboarding);
    }
  }, []);

  const isOnboarding = location.pathname === Path.Onboarding;

  const renderContent = () => {
    if (isAuth) return <AuthPage />;
    if (isOnboarding) return <OnboardingPage />;
    return (
      <>
        <SideBar
          className={clsx({
            [styles["sidebar-show"]]: isHome,
          })}
        />
        <WindowContent>
          <Routes>
            <Route path={Path.Home} element={<Chat />} />
            <Route path={Path.SearchChat} element={<SearchChat />} />
            <Route path={Path.Chat} element={<Chat />} />
            <Route path={Path.Settings} element={<Settings />} />
            <Route path={Path.GatewayControl} element={<GatewayControlPage />} />
            <Route path={Path.SetupWizard} element={<SetupWizardPage />} />
            <Route path={Path.AgentManager} element={<AgentManagerPage />} />
            <Route path={Path.ChannelConfig} element={<ChannelConfigPage />} />
            <Route path={Path.CronManager} element={<CronManagerPage />} />
            <Route path={Path.RsClawPanel} element={<RsClawPanel />} />
            <Route path={Path.Onboarding} element={<OnboardingPage />} />
          </Routes>
        </WindowContent>
      </>
    );
  };

  // Onboarding is a full-screen standalone page -- no container/sidebar.
  if (isOnboarding) {
    return (
      <>
        <OnboardingPage />
        <ToastContainer />
      </>
    );
  }

  return (
    <div
      className={clsx(styles.container, {
        [styles["tight-container"]]: shouldTightBorder,
        [styles["rtl-screen"]]: getLang() === "ar",
      })}
    >
      {renderContent()}
      <ToastContainer />
    </div>
  );
}

export function useLoadData() {
  const config = useAppConfig();

  const api: ClientApi = getClientApi(config.modelConfig.providerName);

  useEffect(() => {
    (async () => {
      const models = await api.llm.models();
      config.mergeModels(models);
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
}

export function Home() {
  useSwitchTheme();
  useLoadData();
  useHtmlLang();

  useEffect(() => {
    console.log("[Config] got config from build time", getClientConfig());
    useAccessStore.getState().fetch();
  }, []);

  if (!useHasHydrated()) {
    return <Loading />;
  }

  return (
    <ErrorBoundary>
      <Router>
        <Screen />
      </Router>
    </ErrorBoundary>
  );
}
