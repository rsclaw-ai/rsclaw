/**
 * Shared restart-in-progress signal.
 *
 * The desktop has two independent gateway status indicators:
 *
 *   1. `<StatusPage>` in rsclaw-panel — the big banner with version, port,
 *      uptime, and Start/Restart/Stop buttons.
 *   2. `<GatewayStatus>` in sidebar — the persistent chip at the bottom of
 *      every page (green dot, "Running" / "Offline" / "Starting…").
 *
 * Both poll `/api/v1/health` on their own intervals (panel: 8s, sidebar: 5s).
 * When the user clicks Restart in the panel, only the panel knows a restart
 * is in flight. The sidebar's next poll lands during the stop+start gap,
 * sees the gateway down, and visibly flips its chip to a non-running state
 * — that's the "离线 → 运行中" flicker.
 *
 * Rather than thread a prop through (the components live on different
 * routes and re-mount independently), we use a module-level pub/sub:
 * whoever initiates a restart calls `setGatewayRestarting(true)`, and both
 * views suppress their offline state until it clears. Race-free as long as
 * one component owns the lifecycle (always the initiator).
 */

let restarting = false;
const listeners = new Set<(v: boolean) => void>();

export function isGatewayRestarting(): boolean {
  return restarting;
}

export function setGatewayRestarting(v: boolean): void {
  if (restarting === v) return;
  restarting = v;
  for (const fn of listeners) fn(v);
}

/** Subscribe to changes. Returns an unsubscribe function. */
export function subscribeGatewayRestart(cb: (v: boolean) => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}
