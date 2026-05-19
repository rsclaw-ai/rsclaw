/**
 * Helpers for the embedded `api.rsclaw.ai/console` webview.
 *
 * The flow:
 *   1. UI calls `openRsclawConsole()` → spawns the child Tauri window
 *      (Rust `open_rsclaw_console` command, see `src-tauri/src/main.rs`).
 *   2. The webview's injected `__RSCLAW_DESKTOP__.installKey(data)` calls
 *      Rust `rsclaw_console_install_key` which re-emits the payload as
 *      a Tauri event `"rsclaw:console-install-key"`.
 *   3. Main-window code subscribes via `onKeyInstalled(handler)` and
 *      applies the key via `applyInstalledKey(data)` — that mutation
 *      lives in TypeScript so we reuse the existing `read_config_file`
 *      + `write_config` round-trip path the rest of onboarding uses.
 *
 * We deliberately don't bundle "open + wait" into one Promise — the
 * webview may sit open for minutes (sign-up email round-trip) while
 * the UI continues to be responsive. Subscribe + close is the right
 * primitive.
 */

import JSON5 from "json5";
import type { UnlistenFn } from "@tauri-apps/api/event";

import { invoke, isTauri } from "../utils/tauri";
import { getConfig, saveConfig, setAuthToken } from "./rsclaw-api";

/** Payload contract with the web team. Mirrors `__RSCLAW_DESKTOP__.installKey`. */
export type InstalledKeyData = {
  /** Human label for the key (e.g. "rsclaw-macos"). Web side defaults
   *  to the hostname we injected via the init script. */
  name?: string;
  /** The full secret, `sk-rsclaw-...`. Required. */
  key: string;
  /** Optional tier label ("free" / "pro" / ...) — purely informational,
   *  used to surface the user's plan in the desktop chip. */
  tier?: string;
};

/**
 * Open (or focus, if already open) the cloud console webview.
 *
 * `path` is appended to the configured base URL so onboarding can
 * deep-link the user straight to `/console/keys` rather than dumping
 * them on a landing page.
 */
export async function openRsclawConsole(opts?: { path?: string }): Promise<void> {
  if (!isTauri) {
    // Browser dev mode: fall back to a normal tab. The install-key
    // callback won't fire (no Tauri channel), so the user has to copy
    // their key manually — which is what the onboarding card's paste
    // fallback is there for.
    const base = "https://api.rsclaw.ai/console";
    const url = opts?.path ? `${base}${opts.path}` : base;
    window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  await invoke("open_rsclaw_console", { path: opts?.path });
}

/** Close the console webview if it's open. No-op in browser dev mode. */
export async function closeRsclawConsole(): Promise<void> {
  if (!isTauri) return;
  try {
    await invoke("close_rsclaw_console");
  } catch {
    /* swallow — fire-and-forget */
  }
}

/**
 * Subscribe to "key installed" events emitted by the webview. The
 * returned function unsubscribes. Safe to call before the webview is
 * opened — the listener stays armed for the next install event.
 *
 * In browser dev mode this is a no-op (returns a no-op unsubscribe)
 * since the Tauri event bus doesn't exist.
 */
export async function onKeyInstalled(
  handler: (data: InstalledKeyData) => void,
): Promise<UnlistenFn> {
  if (!isTauri) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<unknown>("rsclaw:console-install-key", (e) => {
    // The Rust command wraps the data as `{data: ...}` so the web
    // side's `installKey({name, key, tier})` arrives here as either
    // `e.payload` directly (if the wrapper is bypassed) or as
    // `e.payload.data`. Defensive on both shapes.
    const payload = e.payload as { data?: InstalledKeyData } | InstalledKeyData;
    const normalised =
      payload && typeof payload === "object" && "data" in payload
        ? (payload as { data?: InstalledKeyData }).data
        : (payload as InstalledKeyData);
    if (!normalised || typeof normalised.key !== "string" || !normalised.key) {
      console.warn("[rsclaw-console] install event missing key:", e.payload);
      return;
    }
    handler(normalised);
  });
}

/**
 * Persist a freshly-installed key into `rsclaw.json5`.
 *
 * Reads the existing config, merges `models.providers.rsclaw.apiKey`,
 * writes back. Does NOT touch the default model — onboarding's step-2
 * flow remains in charge of which model to set as primary. Caller can
 * also pass `tier` which is stored as `_tier` (underscore-prefixed so
 * the gateway ignores it) purely for desktop UI surfacing later.
 *
 * Returns the merged config object so the caller can refresh derived
 * state (default model dropdown, agents list, …) without re-reading.
 */
export async function applyInstalledKey(
  data: InstalledKeyData,
): Promise<{ ok: boolean; error?: string }> {
  if (!data.key) return { ok: false, error: "missing key" };

  const deepMerge = (dst: any, src: any): any => {
    if (!src || typeof src !== "object" || Array.isArray(src)) return src;
    const result = { ...(dst || {}) };
    for (const [k, v] of Object.entries(src)) {
      if (
        v &&
        typeof v === "object" &&
        !Array.isArray(v) &&
        typeof result[k] === "object" &&
        !Array.isArray(result[k])
      ) {
        result[k] = deepMerge(result[k], v);
      } else {
        result[k] = v;
      }
    }
    return result;
  };

  const patch = {
    models: {
      providers: {
        rsclaw: {
          apiKey: data.key,
          // Stored only for UI surfacing. Prefixed `_` so the gateway
          // config loader (which ignores unknown keys anyway) clearly
          // sees it as desktop metadata, not a provider field.
          _tier: data.tier || undefined,
          _name: data.name || undefined,
        },
      },
    },
  };

  try {
    if (isTauri) {
      // Ensure the config dir exists. This is a no-op if rsclaw is
      // already set up; safe to call from any onboarding step.
      try {
        await invoke("run_setup");
      } catch {
        /* tolerate — config may already be there */
      }

      let existing: any = {};
      try {
        const raw = (await invoke("read_config_file")) as string;
        existing = JSON5.parse(raw || "{}");
      } catch {
        /* missing config → start from empty */
      }
      const merged = deepMerge(existing, patch);
      await invoke("write_config", { content: JSON.stringify(merged, null, 2) });

      // Refresh in-memory auth token if a gateway is already running.
      // Safe to skip if it isn't — the next gateway start picks up the
      // key from rsclaw.json5 directly.
      try {
        const gw: any = await invoke("get_gateway_port");
        if (gw?.token) {
          setAuthToken(gw.token);
          try {
            localStorage.setItem("rsclaw-auth-token", gw.token);
          } catch {
            /* localStorage unavailable */
          }
        }
      } catch {
        /* gateway not running yet — fine */
      }

      return { ok: true };
    }

    // Browser dev mode: write via the HTTP gateway config endpoint.
    let existing: any = {};
    try {
      const cfg = await getConfig();
      existing = JSON5.parse(cfg.raw || "{}");
    } catch {
      /* empty config */
    }
    const merged = deepMerge(existing, patch);
    await saveConfig({ raw: JSON.stringify(merged, null, 2) });
    return { ok: true };
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    return { ok: false, error: msg };
  }
}

/**
 * Light validation of a pasted key. We accept anything that looks
 * roughly like a key prefix the gateway recognises — exact format
 * checks belong on the server side.
 */
export function isLikelyRsclawKey(s: string): boolean {
  const t = s.trim();
  return t.startsWith("sk-rsclaw-") && t.length >= 20;
}
