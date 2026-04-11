import {
  FETCH_COMMIT_URL,
  FETCH_TAG_URL,
  ModelProvider,
  StoreKey,
} from "../constant";
import { getClientConfig } from "../config/client";
import { createPersistStore } from "../utils/store";
import { clientUpdate } from "../utils";
import ChatGptIcon from "../icons/chatgpt.png";
import Locale from "../locales";
import { ClientApi } from "../client/api";

const ONE_MINUTE = 60 * 1000;
const isApp = !!getClientConfig()?.isApp;

function formatVersionDate(t: string) {
  const d = new Date(+t);
  const year = d.getUTCFullYear();
  const month = d.getUTCMonth() + 1;
  const day = d.getUTCDate();

  return [
    year.toString(),
    month.toString().padStart(2, "0"),
    day.toString().padStart(2, "0"),
  ].join("");
}

type VersionType = "date" | "tag";

const RSCLAW_VERSION_URL = "https://app.rsclaw.ai/api/version";

async function getVersion(type: VersionType) {
  // Desktop uses app-v* tags, CLI uses v* tags
  const tagPrefix = isApp ? "app-v" : "v";
  const matchTag = (t: string) => t.startsWith(tagPrefix) && (isApp || !t.startsWith("app-"));

  // Primary: app.rsclaw.ai/api/version (array of release objects)
  try {
    const resp = await fetch(RSCLAW_VERSION_URL, { signal: AbortSignal.timeout(5000) });
    if (resp.ok) {
      const data = await resp.json();
      if (Array.isArray(data)) {
        const release = data.find((r: any) => r.tag_name && matchTag(r.tag_name));
        if (release?.tag_name) return release.tag_name;
      } else if (data?.tag_name) {
        return data.tag_name;
      }
    }
  } catch {}

  // Fallback: GitHub releases API (same array format)
  try {
    const resp = await fetch("https://api.github.com/repos/rsclaw-ai/rsclaw/releases?per_page=10", { signal: AbortSignal.timeout(5000) });
    if (resp.ok) {
      const data = await resp.json();
      if (Array.isArray(data)) {
        const release = data.find((r: any) => r.tag_name && matchTag(r.tag_name));
        if (release?.tag_name) return release.tag_name;
      }
    }
  } catch {}

  // Legacy fallback: GitHub tags
  if (type === "date") {
    const data = (await (await fetch(FETCH_COMMIT_URL)).json()) as {
      commit: {
        author: { name: string; date: string };
      };
      sha: string;
    }[];
    const remoteCommitTime = data[0].commit.author.date;
    const remoteId = new Date(remoteCommitTime).getTime().toString();
    return remoteId;
  } else if (type === "tag") {
    const data = (await (await fetch(FETCH_TAG_URL)).json()) as {
      commit: { sha: string; url: string };
      name: string;
    }[];
    return data.at(0)?.name;
  }
}

export const useUpdateStore = createPersistStore(
  {
    versionType: "tag" as VersionType,
    lastUpdate: 0,
    version: "unknown",
    remoteVersion: "",
    used: 0,
    subscription: 0,

    lastUpdateUsage: 0,
  },
  (set, get) => ({
    formatVersion(version: string) {
      if (get().versionType === "date") {
        version = formatVersionDate(version);
      }
      return version;
    },

    async getLatestVersion(force = false) {
      const versionType = get().versionType;
      let version =
        versionType === "date"
          ? getClientConfig()?.commitDate
          : getClientConfig()?.version;

      set(() => ({ version }));

      const shouldCheck = Date.now() - get().lastUpdate > 2 * 60 * ONE_MINUTE;
      if (!force && !shouldCheck) return;

      set(() => ({
        lastUpdate: Date.now(),
      }));

      try {
        const remoteId = await getVersion(versionType);
        set(() => ({
          remoteVersion: remoteId,
        }));
        if (isApp) {
          try {
            const {
              isPermissionGranted,
              requestPermission,
              sendNotification,
            } = await import("@tauri-apps/plugin-notification");
            const granted = await isPermissionGranted();
            if (!granted) {
              const perm = await requestPermission();
              if (perm !== "granted") return;
            }
            if (version === remoteId) {
              sendNotification({
                title: "RsClaw",
                body: `${Locale.Settings.Update.IsLatest}`,
              });
            } else {
              const updateMessage =
                Locale.Settings.Update.FoundUpdate(`${remoteId}`);
              sendNotification({
                title: "RsClaw",
                body: updateMessage,
              });
              clientUpdate();
            }
          } catch (e) {
            console.warn("Notification error:", e);
          }
        }
        console.log("[Got Upstream] ", remoteId);
      } catch (error) {
        console.error("[Fetch Upstream Commit Id]", error);
      }
    },

    async updateUsage(force = false) {
      // only support openai for now
      const overOneMinute = Date.now() - get().lastUpdateUsage >= ONE_MINUTE;
      if (!overOneMinute && !force) return;

      set(() => ({
        lastUpdateUsage: Date.now(),
      }));

      try {
        const api = new ClientApi(ModelProvider.GPT);
        const usage = await api.llm.usage();

        if (usage) {
          set(() => ({
            used: usage.used,
            subscription: usage.total,
          }));
        }
      } catch (e) {
        console.error((e as Error).message);
      }
    },
  }),
  {
    name: StoreKey.Update,
    version: 1,
  },
);
