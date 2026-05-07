import { useCallback, useEffect, useState } from "react";
import {
  rsclawWs,
  type PermissionRequestPayload,
  type PermissionDecision,
} from "../lib/rsclaw-ws";

/**
 * Subscribes to the gateway's `permission_request` WS frame and exposes
 * the most recent pending request plus a `respond()` helper that sends
 * the user's decision back via `chat.permission_response`.
 *
 * If multiple requests arrive while one is unresolved, the newer one
 * replaces the older — the gateway will time the older request out
 * after 60s anyway.
 */
export type ComputerUsePermissionState = {
  pending: PermissionRequestPayload | null;
};

export type ComputerUsePermissionControls = ComputerUsePermissionState & {
  respond: (decision: PermissionDecision) => Promise<void>;
};

export function useComputerUsePermission(): ComputerUsePermissionControls {
  const [state, setState] = useState<ComputerUsePermissionState>({
    pending: null,
  });

  useEffect(() => {
    rsclawWs.connect();
    const unsub = rsclawWs.onPermissionRequest((payload) => {
      setState({ pending: payload });
    });
    return unsub;
  }, []);

  const respond = useCallback(
    async (decision: PermissionDecision) => {
      const req = state.pending;
      if (!req) return;
      // Clear immediately so a slow gateway doesn't leave the modal
      // mounted while the request is in flight.
      setState({ pending: null });
      try {
        await rsclawWs.permissionResponse(req.request_id, decision, {
          agentId: req.agent_id,
          app: req.app,
        });
      } catch (e) {
        // The driver's polling loop will still time out after 60s if
        // the response is lost — surface a console warning so a
        // developer can see why the dialog vanished without effect.
        console.warn("[permission] response failed:", e);
      }
    },
    [state.pending],
  );

  return { ...state, respond };
}
