import { useEffect, useState } from "react";
import { rsclawWs, type WsConnectionState } from "../lib/rsclaw-ws";

/** Live connection state of the shared gateway WebSocket. */
export function useWsConnectionState(): WsConnectionState {
  const [state, setState] = useState<WsConnectionState>(() =>
    rsclawWs.getState(),
  );
  useEffect(() => {
    rsclawWs.connect();
    setState(rsclawWs.getState());
    return rsclawWs.onStateChange(setState);
  }, []);
  return state;
}
