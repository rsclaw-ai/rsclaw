//! Simple token-bucket rate limiter for WS write operations.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

/// Write methods allowed per minute per client.
pub const WRITE_LIMIT_PER_MINUTE: u32 = 30;

/// Bound on remote peers tracked by the shared limiter.
const MAX_TRACKED_PEERS: usize = 10_000;

/// Write buckets shared by every connection from the same remote IP, so
/// reconnecting does not reset the budget.
static PEER_LIMITERS: LazyLock<Mutex<HashMap<IpAddr, RateLimiter>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Consume one write token from the bucket shared by all connections of
/// `ip`. Returns `true` if allowed.
pub fn check_peer(ip: IpAddr) -> bool {
    let Ok(mut map) = PEER_LIMITERS.lock() else {
        tracing::warn!("ws rate limiter lock poisoned; allowing request");
        return true;
    };
    if !map.contains_key(&ip) && map.len() >= MAX_TRACKED_PEERS {
        // Buckets idle for a full refill interval are back at capacity and
        // carry no state worth keeping.
        map.retain(|_, l| l.last_refill.elapsed() < l.refill_interval);
        if map.len() >= MAX_TRACKED_PEERS {
            let oldest = map
                .iter()
                .min_by_key(|(_, l)| l.last_refill)
                .map(|(k, _)| *k);
            if let Some(k) = oldest {
                map.remove(&k);
            }
        }
    }
    map.entry(ip)
        .or_insert_with(RateLimiter::default_write_limiter)
        .check()
}

/// Per-connection token bucket rate limiter.
///
/// Write methods (sessions.send, chat.send, config.set, etc.) consume one
/// token per call.  Read methods (health, agents.list, etc.) are free.
/// Tokens refill at a fixed rate.
pub struct RateLimiter {
    tokens: u32,
    max_tokens: u32,
    last_refill: Instant,
    refill_interval: Duration,
}

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// `max_tokens` is the burst capacity.  `refill_interval` is how often
    /// the full bucket is restored.  For example, 30 tokens with a 60-second
    /// refill means 30 write requests per minute.
    pub fn new(max_tokens: u32, refill_interval: Duration) -> Self {
        Self {
            tokens: max_tokens,
            max_tokens,
            last_refill: Instant::now(),
            refill_interval,
        }
    }

    /// Default limiter: [`WRITE_LIMIT_PER_MINUTE`] writes per minute.
    pub fn default_write_limiter() -> Self {
        Self::new(WRITE_LIMIT_PER_MINUTE, Duration::from_secs(60))
    }

    /// Try to consume one token.  Returns `true` if allowed, `false` if
    /// rate-limited.
    pub fn check(&mut self) -> bool {
        self.refill();
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self) {
        let elapsed = self.last_refill.elapsed();
        if elapsed >= self.refill_interval {
            self.tokens = self.max_tokens;
            self.last_refill = Instant::now();
        }
    }

    /// Returns true if the given method is a write operation that should
    /// be rate-limited.
    pub fn is_write_method(method: &str) -> bool {
        matches!(
            method,
            "sessions.send"
                | "sessions.create"
                | "sessions.patch"
                | "sessions.compact"
                | "sessions.reset"
                | "sessions.delete"
                | "chat.send"
                | "chat.abort"
                | "agents.create"
                | "agents.update"
                | "agents.delete"
                | "agent.send"
                | "config.set"
                | "config.patch"
                | "config.apply"
                | "cron.add"
                | "cron.remove"
                | "cron.run"
                | "cron.update"
                | "cron.delete"
                | "memory.store"
                | "exec.approval.set"
                | "exec.approval.resolve"
                | "system.shutdown"
                | "system.stop"
                | "system.restart"
                | "system.update.run"
                | "node.pair.request"
                | "node.pair.approve"
                | "node.pair.reject"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max() {
        let mut rl = RateLimiter::new(3, Duration::from_secs(60));
        assert!(rl.check());
        assert!(rl.check());
        assert!(rl.check());
        assert!(!rl.check());
    }

    #[test]
    fn peer_bucket_survives_reconnect() {
        // Documentation-range address, unique to this test.
        let ip: IpAddr = "198.51.100.77".parse().expect("ip");
        for _ in 0..WRITE_LIMIT_PER_MINUTE {
            assert!(check_peer(ip));
        }
        // A new connection from the same IP shares the exhausted bucket.
        assert!(!check_peer(ip));
    }

    #[test]
    fn write_methods_classified() {
        assert!(RateLimiter::is_write_method("sessions.send"));
        assert!(RateLimiter::is_write_method("chat.send"));
        assert!(RateLimiter::is_write_method("config.set"));
        assert!(!RateLimiter::is_write_method("health"));
        assert!(!RateLimiter::is_write_method("agents.list"));
        assert!(!RateLimiter::is_write_method("models.list"));
    }
}
