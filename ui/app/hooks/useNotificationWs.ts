import { useEffect, useRef } from "react";
import { getGatewayUrl, getAuthToken } from "../lib/rsclaw-api";
import { showToast } from "../components/ui-lib";

/**
 * Maintains a WebSocket connection to the gateway for receiving
 * push notifications (cron reminders, system alerts, etc.).
 *
 * Reconnects automatically on disconnect with exponential backoff.
 */
export function useNotificationWs() {
  const wsRef = useRef<WebSocket | null>(null);
  const retryRef = useRef(0);

  useEffect(() => {
    let mounted = true;
    let timer: ReturnType<typeof setTimeout>;

    function connect() {
      if (!mounted) return;

      const gwUrl = getGatewayUrl() || "http://localhost:18888";
      const wsUrl = gwUrl.replace(/^http/, "ws") + "/ws";
      const token = getAuthToken();

      try {
        const ws = new WebSocket(wsUrl);
        wsRef.current = ws;

        ws.onopen = () => {
          retryRef.current = 0;
          // Wait for connect.challenge before sending connect req
        };

        ws.onmessage = (event) => {
          try {
            const data = JSON.parse(event.data);

            // Step 1: server sends connect.challenge — respond with connect req
            if (data.event === "connect.challenge") {
              const connectReq = {
                type: "req",
                id: "1",
                method: "connect",
                params: {
                  client: "desktop-ui",
                  min_protocol: 3,
                  max_protocol: 3,
                  auth: token ? { token } : undefined,
                },
              };
              ws.send(JSON.stringify(connectReq));
              return;
            }

            // Handle notification events from cron/system
            if (data.event === "notification" || data.type === "notification") {
              const text =
                data.payload?.text ||
                data.data?.text ||
                data.text ||
                "";
              if (text) {
                showToast(text, undefined, 10000);
                if (Notification?.permission === "granted") {
                  new Notification("RsClaw", { body: text });
                }
              }
            }
          } catch {
            // ignore non-JSON messages
          }
        };

        ws.onclose = () => {
          wsRef.current = null;
          if (!mounted) return;
          // Reconnect with backoff: 1s, 2s, 4s, 8s, ... max 30s
          const delay = Math.min(1000 * Math.pow(2, retryRef.current), 30000);
          retryRef.current++;
          timer = setTimeout(connect, delay);
        };

        ws.onerror = () => {
          // onclose will fire after onerror
        };
      } catch {
        // WebSocket constructor can throw if URL is invalid
        const delay = Math.min(1000 * Math.pow(2, retryRef.current), 30000);
        retryRef.current++;
        timer = setTimeout(connect, delay);
      }
    }

    // Request browser notification permission
    if (typeof Notification !== "undefined" && Notification.permission === "default") {
      Notification.requestPermission();
    }

    connect();

    return () => {
      mounted = false;
      clearTimeout(timer);
      if (wsRef.current) {
        wsRef.current.close();
        wsRef.current = null;
      }
    };
  }, []);
}
