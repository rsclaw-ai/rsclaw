import { useEffect } from "react";
import { rsclawWs } from "../lib/rsclaw-ws";
import { showToast } from "../components/ui-lib";
import { useChatStore, createMessage } from "../store";

/**
 * Maintains a WebSocket connection to the gateway for receiving
 * push notifications (cron reminders, system alerts, etc.).
 *
 * Uses the shared rsclawWs singleton so the same connection is reused
 * by the chat path in openai.ts.
 */
export function useNotificationWs() {
  const chatStore = useChatStore();

  useEffect(() => {
    // Request browser notification permission
    if (
      typeof Notification !== "undefined" &&
      Notification.permission === "default"
    ) {
      Notification.requestPermission();
    }

    rsclawWs.connect();

    const unsub = rsclawWs.onNotification((text, kind, images) => {
      // Resolve a human-readable label for the kind tag.
      // task_complete   = async /task agent finished
      // async_send      = async /send agent finished
      // (undefined)     = ordinary notification (cron, system alert, etc.)
      const badge =
        kind === "task_complete"
          ? "[任务完成] "
          : kind === "async_send"
            ? "[异步发送] "
            : "";
      const labeled = badge ? `${badge}${text}` : text;
      const hasImages = !!images && images.length > 0;
      // Image-only pushes (e.g. the astock chart PNG) carry no text — give
      // the toast/native popup a placeholder so they still surface.
      const display = labeled || (hasImages ? "[图片]" : "");

      // Toast
      if (display) showToast(display, undefined, 10000);

      // Native notification
      if (display && Notification?.permission === "granted") {
        new Notification("RsClaw", { body: display });
      }

      // Also add to current chat session so it's visible inline.
      // When the push carries images (chart PNG, login QR, ...), build a
      // multimodal content array so the chat renderer shows the image
      // bubble — and render it as a real assistant reply (not the muted
      // intermediate palette), matching how channels like Feishu display it.
      // Pure-text notifications stay intermediate (progress chatter).
      const content = hasImages
        ? [
            ...(labeled ? [{ type: "text" as const, text: labeled }] : []),
            ...images!.map((url) => ({
              type: "image_url" as const,
              image_url: { url },
            })),
          ]
        : labeled;

      const session = useChatStore.getState().currentSession();
      useChatStore.getState().updateTargetSession(session, (s) => {
        s.messages.push(
          createMessage({
            role: "assistant",
            content,
            isIntermediate: !hasImages,
          }),
        );
      });
    });

    return unsub;
  }, []);
}
