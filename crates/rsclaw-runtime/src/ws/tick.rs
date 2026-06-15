//! 15-second tick broadcaster — sends a heartbeat event to all connected
//! WebSocket clients at a fixed interval.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use serde_json::json;

use super::{conn::ConnRegistry, types::EventFrame};

pub fn start_tick_loop(conns: Arc<ConnRegistry>) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15));
        loop {
            interval.tick().await;
            let ts_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let frame = EventFrame::new("tick", json!({ "ts": ts_ms }), 0);
            conns.broadcast_all(frame).await;
        }
    });
}
