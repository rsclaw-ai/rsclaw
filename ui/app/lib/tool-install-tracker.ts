/**
 * Module-level tracker for in-flight `rsclaw tools install <name>` jobs.
 *
 * The Rust side spawns the install detached and returns immediately
 * (fire-and-forget); the frontend polls `get_tool_install_status` until
 * the binary appears under `~/.rsclaw/tools/<name>/` or a timeout fires.
 *
 * Earlier the polling loop + spinner state lived inside `ToolsTab`'s
 * local React state. Switching to another panel tab unmounted the
 * component, the closure died with it, and on re-mount the new tab
 * showed Install button instead of "Installing…" even though the
 * rsclaw subprocess was still happily downloading. State lives here
 * now so the spinner persists across mount cycles.
 *
 * Pub/sub mirrors the gateway-restart-signal pattern — one shared Set,
 * subscribers re-render via a forced state bump. Cheaper than a context
 * for what's effectively a singleton.
 */

import { useEffect, useState } from "react";

import { getLang } from "../locales";
import { invoke as tauriInvoke } from "../utils/tauri";
import { toast } from "./toast";

const installing = new Set<string>();
const subscribers = new Set<() => void>();

function notify(): void {
  for (const cb of subscribers) cb();
}

/**
 * Subscribe a component to the in-flight install set. Returns a snapshot
 * Set; re-renders on every change so `installing.has(name)` reads stay
 * fresh.
 */
export function useInstallingTools(): Set<string> {
  const [, force] = useState(0);
  useEffect(() => {
    const cb = () => force((n) => n + 1);
    subscribers.add(cb);
    return () => {
      subscribers.delete(cb);
    };
  }, []);
  return new Set(installing);
}

const POLL_INTERVAL_MS = 1500;
const TIMEOUT_MS = 10 * 60 * 1000;

/**
 * Kick off (or rejoin) install for one tool.
 *
 * Idempotent — calling a second time while an install is already in
 * flight is a no-op so we don't double-poll on tab re-mount. Caller
 * can pass `onComplete` to refresh their UI on success; if the calling
 * component has unmounted by then the callback gets dropped and the
 * next mount's `useEffect` picks up the new state on its own.
 */
export async function startToolInstall(
  name: string,
  onComplete?: () => void | Promise<void>,
): Promise<void> {
  if (installing.has(name)) return;
  installing.add(name);
  notify();

  const zh = getLang() === "cn";
  try {
    await tauriInvoke("install_tool", { name, force: true });
    const startedAt = Date.now();
    // Poll the background install OUTCOME (not just the file probe). The
    // outcome carries the sidecar's real stdout+stderr, so on failure we can
    // show the actual reason (no download for platform / download failed /
    // extract error / no runnable binary) instead of a generic timeout.
    while (Date.now() - startedAt < TIMEOUT_MS) {
      await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
      // External cancel — abandon the poll without surfacing an error.
      if (!installing.has(name)) return;

      const res = (await tauriInvoke("get_tool_install_result", {
        name,
      })) as { done?: boolean; success?: boolean; output?: string };

      if (res?.done) {
        if (res.success) {
          // Sidecar reported success — double-check the binary actually landed.
          const status = (await tauriInvoke("get_tool_install_status", {
            name,
          })) as { installed?: boolean };
          if (!status?.installed) {
            throw new Error(
              (zh
                ? "安装命令成功但未检测到二进制：\n"
                : "Install reported success but no binary was found:\n") +
                (res.output || "").trim(),
            );
          }
          try {
            await onComplete?.();
          } catch {
            /* tolerate — refresh failure shouldn't error the install path */
          }
          toast.success(zh ? `${name} 安装完成` : `${name} installed`);
          return;
        }
        // Failed — surface the sidecar's real error text.
        throw new Error((res.output || "").trim() || (zh ? "安装失败（无输出）" : "Install failed (no output)"));
      }
    }
    // Hit the wall-clock ceiling without the sidecar finishing.
    throw new Error(
      zh
        ? "安装超时（10 分钟）。详细日志见 ~/.rsclaw/var/tools-install.log"
        : "Install timed out (10min). See ~/.rsclaw/var/tools-install.log",
    );
  } catch (e: unknown) {
    const msg = typeof e === "string" ? e : (e as Error)?.message || "";
    const hint = zh
      ? "\n详见 ~/.rsclaw/var/tools-install.log"
      : "\nSee ~/.rsclaw/var/tools-install.log";
    toast.fromError(zh ? "安装失败" : "Install failed", `${msg}${hint}`);
  } finally {
    installing.delete(name);
    notify();
  }
}

/** Cancel polling for a name. Doesn't kill the rsclaw subprocess — the
 *  install may still complete on disk; we just stop watching. */
export function cancelToolInstall(name: string): void {
  if (!installing.has(name)) return;
  installing.delete(name);
  notify();
}
