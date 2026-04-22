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

    const unsub = rsclawWs.onNotification((text) => {
      // Toast
      showToast(text, undefined, 10000);

      // Native notification
      if (Notification?.permission === "granted") {
        new Notification("RsClaw", { body: text });
      }

      // Also add to current chat session so it's visible inline
      const session = useChatStore.getState().currentSession();
      useChatStore.getState().updateTargetSession(session, (s) => {
        s.messages.push(
          createMessage({
            role: "assistant",
            content: text,
          }),
        );
      });
    });

    return unsub;
  }, []);
}
