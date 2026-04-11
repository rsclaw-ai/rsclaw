// Tauri v2 utility module — centralizes all Tauri API imports.
// Components should import from here instead of using window.__TAURI__.

import { invoke as tauriInvoke } from "@tauri-apps/api/core";
import { listen as tauriListen } from "@tauri-apps/api/event";

/** True when running inside Tauri desktop app. */
export const isTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

/** Call a Tauri command. Returns undefined in browser. */
// eslint-disable-next-line @typescript-eslint/no-explicit-any
export async function invoke(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<any> {
  return tauriInvoke(cmd, args);
}

/** Listen to a Tauri event. Returns unlisten function. */
export const listen = tauriListen;
