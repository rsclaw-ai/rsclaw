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

/**
 * Payload of a `permission_request` frame, mirrors
 * `crate::computer::permission::PermissionRequest`. Surfaced to the UI
 * as a red-accented consent modal before any GUI agent loop touches the
 * desktop.
 */
export type PermissionRequestPayload = {
  request_id: string;
  agent_id: string;
  /** Display name of the app being controlled (may be empty). */
  app: string;
  /** Plain-language summary of what the agent is about to do. */
  reason: string;
  /** Estimated max action count (`max_loop`). */
  estimated_steps: number;
};

/** Decision the user picks in the permission dialog. */
export type PermissionDecision =
  | "allow_once"
  | "allow_session"
  | "allow_always"
  | "deny";

/**
 * Payload of a `computer_use_status` frame, mirrors
 * `crate::computer::status::ComputerUseStatus`. The discriminator is
 * `kind` (snake_case). Surfaced to the live status panel in the
 * settings UI so the user can see what the GUI agent is doing.
 */
export type ComputerUseStatusPayload =
  | {
      kind: "started";
      run_id: string;
      agent_id: string;
      app: string;
      instruction: string;
      max_steps: number;
    }
  | {
      kind: "step";
      run_id: string;
      step_index: number;
      action_summary: string;
      thought: string;
      result_ok: boolean;
      result_message: string | null;
    }
  | {
      kind: "finished";
      run_id: string;
      outcome_kind:
        | "finished"
        | "call_user"
        | "max_loop"
        | "user_abort"
        | "permission_denied"
        | "operator_error";
      steps: number;
      summary: string;
    };

/**
 * One option in an `AskUserPrompt`. Field names match the on-wire
 * snake_case shape — no rename needed on the backend.
 */
export type AskUserOption = {
  /** Display label, 1–5 words. */
  label: string;
  /** Optional one-line elaboration shown beneath the label. */
  description?: string;
};

/**
 * Payload of an `ask_user` frame (either nested inside a `chat` frame as
 * `p.type === "ask_user"` or as a standalone `session.ask_user` frame).
 * Mirrors `crate::events::AskUserPrompt`. Surfaced to the UI as a
 * non-destructive modal so the agent can collect a structured choice
 * mid-turn.
 */
export type AskUserPrompt = {
  question: string;
  options: AskUserOption[];
  /** Default false — single-select. */
  multi_select?: boolean;
  /** 0-based index of the agent-recommended option. */
  recommended_index?: number;
  /** Optional short label rendered as a chip before the question. */
  header?: string;
};

/**
 * Envelope wrapping an `AskUserPrompt` with the routing fields the UI
 * needs to know which chat the question belongs to. Both relay paths
 * (`chat` payload with `p.type === "ask_user"` and the standalone
 * `session.ask_user` frame) get normalised into this shape before the
 * handler set is fanned out.
 */
export type AskUserPayload = {
  /** Always present from at least one relay path. */
  sessionKey: string;
  /** Only set when the relay path is `chat` (HTTP-initiated). */
  runId?: string;
  agentId?: string;
  prompt: AskUserPrompt;
};

/** Payload of a `restart.required` frame, mirrors src/events.rs RestartRequest. */
export type RestartRequiredPayload = {
  at_ms: number;
  /** RestartReason — { kind: "config_changed" | ... } */
  reason: { kind: string; [key: string]: unknown };
  urgency: "recommended" | "required";
  /** Pre-translated message from the gateway. */
  message: string;
  /**
   * Inflight work count when the event was published. `0` means the gateway
   * is idle and the UI should restart immediately (no countdown). When `> 0`,
   * the backend re-publishes with `inflight = 0` once the gateway drains
   * (max 60s), so the UI can short-circuit its countdown on the follow-up.
   * Optional for backward compatibility with older gateways.
   */
  inflight?: number;
};

class RsClawWsClient {
  private ws: WebSocket | null = null;
  private retryCount = 0;
  private retryTimer: ReturnType<typeof setTimeout> | null = null;
  private mounted = true;

  private reqCounter = 2; // 1 is reserved for the connect handshake
  private pendingReqs = new Map<string, PendingReq>();
  private chatHandlers = new Map<string, { cb: ChatCallbacks; fullText: string }>();
  private notificationHandlers = new Set<(text: string, kind?: string) => void>();
  private restartHandlers = new Set<(payload: RestartRequiredPayload) => void>();
  private permissionHandlers = new Set<(payload: PermissionRequestPayload) => void>();
  private statusHandlers = new Set<(payload: ComputerUseStatusPayload) => void>();
  private askUserHandlers = new Set<(payload: AskUserPayload) => void>();
  private connectHandlers = new Set<() => void>();
  private tokenRefresh: Promise<void> | null = null;

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
  onNotification(handler: (text: string, kind?: string) => void): () => void {
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

  /**
   * Register a handler for `permission_request` event frames the gateway
   * emits before driving the desktop with the GUI agent. The handler
   * should mount a consent UI; the user's choice is sent back via
   * `chat.permission_response`. Returns an unsubscribe function.
   */
  onPermissionRequest(
    handler: (payload: PermissionRequestPayload) => void,
  ): () => void {
    this.permissionHandlers.add(handler);
    return () => this.permissionHandlers.delete(handler);
  }

  /**
   * Register a handler for `computer_use_status` event frames the
   * gateway broadcasts as the GUI agent progresses through its loop.
   * Returns an unsubscribe function.
   */
  onComputerUseStatus(
    handler: (payload: ComputerUseStatusPayload) => void,
  ): () => void {
    this.statusHandlers.add(handler);
    return () => this.statusHandlers.delete(handler);
  }

  /**
   * Register a handler for `ask_user` prompts the agent emits when it
   * wants a structured choice from the user mid-turn. Both the
   * `chat`-payload relay (`p.type === "ask_user"`) and the standalone
   * `session.ask_user` frame are normalised into one `AskUserPayload`
   * before being fanned out here. Returns an unsubscribe function.
   *
   * Note: the agent will ALSO stream a numbered-options fallback as
   * regular text-delta frames around this event. Suppressing that
   * fallback in the transcript is up to the consumer — by default it
   * remains visible so the question is still in history if the modal
   * is cancelled.
   */
  onAskUser(
    handler: (payload: AskUserPayload) => void,
  ): () => void {
    this.askUserHandlers.add(handler);
    return () => this.askUserHandlers.delete(handler);
  }

  /**
   * Reply to a `PermissionRequest` with the user's decision. Returns
   * the gateway's `{ resolved, requestId }` response.
   */
  permissionResponse(
    requestId: string,
    decision: PermissionDecision,
    extra?: { agentId?: string; app?: string },
  ): Promise<{ resolved: boolean; requestId: string }> {
    return this.send("chat.permission_response", {
      requestId,
      decision,
      agentId: extra?.agentId,
      app: extra?.app,
    });
  }

  /**
   * Register a handler fired after each successful connect handshake.
   * Use this to reset transient client state (e.g. dismiss the restart
   * banner) so a fresh gateway with an empty latch doesn't inherit stale
   * UI from the previous session. If the new gateway has a latched
   * `restart.required`, it arrives immediately after this fires and re-arms
   * the banner. Returns an unsubscribe function.
   */
  onConnect(handler: () => void): () => void {
    this.connectHandlers.add(handler);
    return () => this.connectHandlers.delete(handler);
  }

  private async _doConnect() {
    if (this.retryTimer) {
      clearTimeout(this.retryTimer);
      this.retryTimer = null;
    }

    // Wait for any pending token refresh before reconnecting.
    if (this.tokenRefresh) {
      await this.tokenRefresh;
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
    this.tokenRefresh = tauriInvoke("get_gateway_port")
      .then((gw: any) => {
        if (gw?.token) {
          setAuthToken(gw.token);
          try {
            localStorage.setItem("rsclaw-auth-token", gw.token);
          } catch {}
          console.info("[rsclaw-ws] auth token refreshed from config");
        }
      })
      .catch((e: unknown) => {
        console.warn("[rsclaw-ws] token refresh failed:", e);
      })
      .finally(() => {
        this.tokenRefresh = null;
      });
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
        // Notify subscribers that a fresh handshake completed. Any latched
        // events (e.g. restart.required) arrive in subsequent frames, so
        // subscribers can safely reset state here without losing real events.
        this.connectHandlers.forEach((h) => {
          try {
            h();
          } catch {
            // handler errors must not break the WS pipeline
          }
        });
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
      const kind: string | undefined =
        data.payload?.kind || data.data?.kind || data.kind || undefined;
      if (text) {
        this.notificationHandlers.forEach((h) => h(text, kind));
      }
      return;
    }

    if (event === "restart.required") {
      const payload = (data.payload || data.data || {}) as RestartRequiredPayload;
      this.restartHandlers.forEach((h) => h(payload));
      return;
    }

    if (event === "permission_request") {
      const payload = (data.payload || data.data || {}) as PermissionRequestPayload;
      this.permissionHandlers.forEach((h) => h(payload));
      return;
    }

    if (event === "computer_use_status") {
      const payload = (data.payload || data.data || {}) as ComputerUseStatusPayload;
      this.statusHandlers.forEach((h) => h(payload));
      return;
    }

    // Standalone session-scoped relay (WS-initiated chats subscribing
    // to a session). Frame shape: { type: "session.ask_user", data: {
    // sessionKey, agentId, prompt } }.
    if (event === "session.ask_user") {
      const d = data.payload || data.data || {};
      const prompt = (d.prompt || {}) as AskUserPrompt;
      if (!prompt.question) return;
      this.askUserHandlers.forEach((h) =>
        h({
          sessionKey: d.sessionKey || d.session_key || "",
          agentId: d.agentId || d.agent_id,
          prompt,
        }),
      );
      return;
    }

    if (event === "chat") {
      const p = data.payload || {};
      const runId: string = p.runId || "";

      // `ask_user` rides on the chat envelope but is independent of
      // any `runId`-keyed streaming handler — it's a sideband prompt
      // the modal subscribes to globally. Fan out to ask-user
      // handlers regardless of whether a chat callback is registered
      // for this runId.
      if (p.type === "ask_user") {
        const prompt = (p.prompt || {}) as AskUserPrompt;
        if (prompt.question) {
          this.askUserHandlers.forEach((h) =>
            h({
              sessionKey: p.sessionKey || p.session_key || "",
              runId,
              agentId: p.agentId || p.agent_id,
              prompt,
            }),
          );
        }
        return;
      }

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
