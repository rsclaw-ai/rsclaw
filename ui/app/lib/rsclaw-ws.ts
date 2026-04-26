/**
 * Singleton WebSocket client for the RsClaw gateway (protocol v3).
 *
 * Shared between useNotificationWs (notifications) and openai.ts (chat).
 * Handles connection lifecycle, challenge/response handshake, and event routing.
 */

import { getGatewayUrl, getAuthToken, setAuthToken } from "./rsclaw-api";

export type ChatCallbacks = {
  onDelta: (fullText: string, delta: string) => void;
  onDone: (
    files: [string, string, string][],
    images: string[],
    toolLog: [string, string, string][],
  ) => void;
  onError: (err: Error) => void;
};

type PendingReq = {
  resolve: (value: any) => void;
  reject: (reason: any) => void;
};

/** Payload of a `restart.required` frame, mirrors src/events.rs RestartRequest. */
export type RestartRequiredPayload = {
  at_ms: number;
  /** RestartReason — { kind: "config_changed" | "model_downloaded" | ... } */
  reason: { kind: string; [key: string]: unknown };
  urgency: "recommended" | "required";
  /** Pre-translated message from the gateway. */
  message: string;
};

class RsClawWsClient {
  private ws: WebSocket | null = null;
  private retryCount = 0;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private mounted = true;

  private reqCounter = 2; // 1 is reserved for the connect handshake
  private pendingReqs = new Map<string, PendingReq>();
  private chatHandlers = new Map<string, { cb: ChatCallbacks; fullText: string }>();
  private notificationHandlers = new Set<(text: string) => void>();
  private restartHandlers = new Set<(payload: RestartRequiredPayload) => void>();

  /** Ensure the WS is connected. Safe to call multiple times. */
  connect() {
    if (
      this.ws &&
      (this.ws.readyState === WebSocket.OPEN ||
        this.ws.readyState === WebSocket.CONNECTING)
    ) {
      return;
    }
    this._doConnect();
  }

  /** Send a method request, returns the response payload. */
  send(method: string, params: Record<string, unknown>): Promise<any> {
    return new Promise((resolve, reject) => {
      if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
        reject(new Error("WebSocket not connected"));
        return;
      }
      const id = String(this.reqCounter++);
      this.pendingReqs.set(id, { resolve, reject });
      this.ws.send(
        JSON.stringify({ type: "req", id, method, params }),
      );
    });
  }

  /** Register chat-event callbacks for a specific runId. */
  onChatEvent(runId: string, cb: ChatCallbacks) {
    this.chatHandlers.set(runId, { cb, fullText: "" });
  }

  /** Register a notification handler. Returns an unsubscribe function. */
  onNotification(handler: (text: string) => void): () => void {
    this.notificationHandlers.add(handler);
    return () => this.notificationHandlers.delete(handler);
  }

  /**
   * Register a handler for `restart.required` event frames. The gateway
   * latches the most recent pending request, so a handler registered after
   * the frame was emitted still receives it on the next reconnect handshake.
   * Returns an unsubscribe function.
   */
  onRestartRequired(
    handler: (payload: RestartRequiredPayload) => void,
  ): () => void {
    this.restartHandlers.add(handler);
    return () => this.restartHandlers.delete(handler);
  }

  private _doConnect() {
    if (this.retryTimer) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }

    const gwUrl = getGatewayUrl() || "http://localhost:18888";
    const wsUrl = gwUrl.replace(/^http/, "ws") + "/ws";

    try {
      const ws = new WebSocket(wsUrl);
      this.ws = ws;

      ws.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data as string);
          this._handleFrame(data);
        } catch {
          // ignore non-JSON
        }
      };

      ws.onclose = () => {
        this.ws = null;
        // Reject all pending requests
        this.pendingReqs.forEach(({ reject }) =>
          reject(new Error("WebSocket closed")),
        );
        this.pendingReqs.clear();
        this._scheduleReconnect();
      };

      ws.onerror = () => {
        // onclose fires after onerror
      };
    } catch {
      this._scheduleReconnect();
    }
  }

  private _scheduleReconnect() {
    if (!this.mounted) return;
    const delay = Math.min(1000 * Math.pow(2, this.retryCount), 30000);
    this.retryCount++;
    this.retryTimer = setTimeout(() => this._doConnect(), delay);
  }

  /** Re-read auth token from Tauri config on auth failure. */
  private _refreshTokenFromTauri() {
    const tauriInvoke = (window as any).__TAURI__?.invoke;
    if (!tauriInvoke) return;
    tauriInvoke("get_gateway_port")
      .then((gw: any) => {
        if (gw?.token) {
          setAuthToken(gw.token);
          try {
            localStorage.setItem("rsclaw-auth-token", gw.token);
          } catch {}
          console.info("[rsclaw-ws] auth token refreshed from config");
        }
      })
      .catch(() => {});
  }

  private _handleFrame(data: any) {
    // Challenge/response handshake
    if (data.event === "connect.challenge") {
      const token = getAuthToken();
      this.ws?.send(
        JSON.stringify({
          type: "req",
          id: "1",
          method: "connect",
          params: {
            client: { id: "rsclaw:desktop", version: "dev", platform: "tauri", mode: "ui" },
            minProtocol: 3,
            maxProtocol: 3,
            auth: token ? { token } : undefined,
          },
        }),
      );
      return;
    }

    // Connected confirmation
    if (data.type === "res" && data.id === "1") {
      if (data.ok) {
        this.retryCount = 0;
      } else {
        // Auth failed — refresh token from Tauri config before next retry.
        console.warn("[rsclaw-ws] connect failed:", data.error?.message);
        this._refreshTokenFromTauri();
      }
      return;
    }

    // Response to a pending request
    if (data.type === "res") {
      const pending = this.pendingReqs.get(data.id);
      if (pending) {
        this.pendingReqs.delete(data.id);
        if (data.ok) {
          pending.resolve(data.payload);
        } else {
          pending.reject(new Error(data.error?.message || "request failed"));
        }
      }
      return;
    }

    // Event frames
    const event = data.event || data.type;

    if (event === "notification") {
      const text =
        data.payload?.text || data.data?.text || data.text || "";
      if (text) {
        this.notificationHandlers.forEach((h) => h(text));
      }
      return;
    }

    if (event === "restart.required") {
      const payload = (data.payload || data.data || {}) as RestartRequiredPayload;
      this.restartHandlers.forEach((h) => h(payload));
      return;
    }

    if (event === "chat") {
      const p = data.payload || {};
      const runId: string = p.runId || "";
      const entry = this.chatHandlers.get(runId);
      if (!entry) return;

      if (p.type === "text_delta") {
        entry.fullText += p.delta || "";
        entry.cb.onDelta(entry.fullText, p.delta || "");
      } else if (p.type === "done") {
        const files: [string, string, string][] = p.files || [];
        const images: string[] = p.images || [];
        const toolLog: [string, string, string][] = p.toolLog || [];
        this.chatHandlers.delete(runId);
        entry.cb.onDone(files, images, toolLog);
      }
    }
  }

  destroy() {
    this.mounted = false;
    if (this.retryTimer) clearTimeout(this.retryTimer);
    this.ws?.close();
    this.ws = null;
  }
}

export const rsclawWs = new RsClawWsClient();
