// Tauri v2 utility module — centralizes all Tauri API imports.
// Components should import from here instead of using window.__TAURI__.

import {
  invoke as tauriInvoke,
  convertFileSrc as tauriConvertFileSrc,
} from "@tauri-apps/api/core";
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

/**
 * Convert an absolute local filesystem path into an `asset://` URL the
 * Tauri webview can load (images, video, audio). Runtime check here instead
 * of relying on the module-level `isTauri` const — that one is frozen to
 * `false` during Next.js SSR because `window` is undefined on the server,
 * and even post-hydration some callers end up with the stale value.
 */
export function convertFileSrc(path: string, protocol = "asset"): string {
  if (
    typeof window === "undefined" ||
    !("__TAURI_INTERNALS__" in window)
  ) {
    return path;
  }
  return tauriConvertFileSrc(path, protocol);
}
