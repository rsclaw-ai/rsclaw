/**
 * Shared poller for `GET /api/v1/models/health`.
 *
 * Every <ModelChainInput> renders a status dot per model in the chain, and
 * those dots all read the same gateway snapshot. Rather than have N inputs
 * each spin up their own setInterval, we keep one module-level poller +
 * pub/sub. `useModelHealth()` subscribes the calling component to the
 * latest snapshot and starts the poller on first mount; the poller stops
 * when the last subscriber unmounts.
 *
 * Gateway down / endpoint 404 → silent failure, cache stays empty, dots
 * render gray. We don't want the model-config page to throw toasts while
 * the user is just typing.
 */

import { useEffect, useState } from "react";

import {
  getModelHealth,
  resetModelHealth,
  type ModelHealthEntry,
} from "./rsclaw-api";

let cache: Record<string, ModelHealthEntry> = {};
const subscribers = new Set<() => void>();
let pollTimer: ReturnType<typeof setInterval> | null = null;
let activeRefs = 0;

const POLL_INTERVAL_MS = 5000;

async function refresh(): Promise<void> {
  try {
    const r = await getModelHealth();
    const next: Record<string, ModelHealthEntry> = {};
    for (const m of r?.models || []) {
      if (m && typeof m.model === "string") next[m.model] = m;
    }
    cache = next;
    for (const cb of subscribers) cb();
  } catch {
    // gateway not up, endpoint missing, or auth expired — render gray dots
    // and try again on the next tick. Don't surface toast spam.
  }
}

function startPolling(): void {
  activeRefs += 1;
  if (pollTimer) return;
  void refresh();
  pollTimer = setInterval(() => void refresh(), POLL_INTERVAL_MS);
}

function stopPolling(): void {
  activeRefs = Math.max(0, activeRefs - 1);
  if (activeRefs > 0) return;
  if (pollTimer) clearInterval(pollTimer);
  pollTimer = null;
}

/**
 * Subscribe a component to the model-health snapshot. Returns the current
 * map (model_id → entry). Components re-render on every refresh tick.
 */
export function useModelHealth(): Record<string, ModelHealthEntry> {
  const [, force] = useState(0);
  useEffect(() => {
    startPolling();
    const cb = () => force((x) => x + 1);
    subscribers.add(cb);
    return () => {
      subscribers.delete(cb);
      stopPolling();
    };
  }, []);
  return cache;
}

/**
 * Reset a model's Disabled/Cooling state on the gateway and refresh the
 * shared snapshot. Used by the dot's click-to-reset affordance.
 *
 * Resolves to `true` when the gateway acknowledged the reset, `false` on
 * 404 / network failure. Caller decides whether to surface a toast.
 */
export async function resetAndRefreshModelHealth(
  model: string,
): Promise<boolean> {
  try {
    const r = await resetModelHealth(model);
    await refresh();
    return !!r?.ok;
  } catch {
    return false;
  }
}
