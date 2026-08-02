//! Rate limiting.
//!
//! Gal's expensive endpoints are cheap to *call*: a login costs an Argon2 hash
//! (tens of milliseconds of CPU and ~19 MiB), and registration additionally
//! writes a row. Without a limiter, a single client can saturate every core —
//! measured at ~500 concurrent logins driving unrelated request latency from
//! 0.2 ms to over a second.
//!
//! This is a token bucket keyed by client identity, held in memory. It is not a
//! distributed limiter: with several instances behind a load balancer each keeps
//! its own counts, which is fine for abuse control and is not a security
//! boundary on its own.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A refilling allowance for one key.
#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
    capacity: f64,
    refill_per_sec: f64,
    /// Stop tracking a key once it has been idle this long.
    idle_eviction: Duration,
}

impl RateLimiter {
    /// `capacity` requests in a burst, refilling at `per_sec`.
    pub fn new(capacity: f64, per_sec: f64) -> Self {
        RateLimiter {
            buckets: Mutex::new(HashMap::new()),
            capacity,
            refill_per_sec: per_sec,
            idle_eviction: Duration::from_secs(600),
        }
    }

    /// Take one token. `false` means the caller is over their allowance.
    pub fn check(&self, key: &str) -> bool {
        self.check_cost(key, 1.0)
    }

    /// Take `cost` tokens, for a surface where calls are not all worth the same.
    ///
    /// A WebSocket command can be a cursor position or it can be a replay of
    /// twenty thousand rows off the disk; one bucket with one price per call
    /// would have to be set for the second and would then be useless against a
    /// flood of the first.
    ///
    /// A caller with fewer than `cost` tokens is refused *and charged nothing*,
    /// so an expensive call it cannot afford does not also drain what it needs
    /// for the cheap ones.
    pub fn check_cost(&self, key: &str, cost: f64) -> bool {
        self.take(key, cost, true)
    }

    /// Whether a call *would* be allowed, taking nothing.
    ///
    /// For a limiter charged only on failure: the check has to happen before
    /// the expensive work, and the charge only after it turns out to have been
    /// wasted. Peeking with a cost of zero would not do — an empty bucket has
    /// zero tokens, and zero is not less than zero.
    pub fn would_allow(&self, key: &str) -> bool {
        self.take(key, 1.0, false)
    }

    fn take(&self, key: &str, cost: f64, consume: bool) -> bool {
        let now = Instant::now();
        let mut buckets = match self.buckets.lock() {
            Ok(b) => b,
            // A poisoned lock must not take the server down; fail open rather
            // than lock everyone out of logging in.
            Err(poisoned) => poisoned.into_inner(),
        };

        // Opportunistic cleanup so the map cannot grow without bound from a
        // stream of distinct source addresses.
        if buckets.len() > 4096 {
            let idle = self.idle_eviction;
            buckets.retain(|_, b| now.duration_since(b.last) < idle);
        }

        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: self.capacity,
            last: now,
        });

        let elapsed = now.duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last = now;

        if bucket.tokens >= cost {
            if consume {
                bucket.tokens -= cost;
            }
            true
        } else {
            false
        }
    }
}

/// Best-effort client identity for rate-limiting purposes.
///
/// Behind a reverse proxy the peer address is the proxy, so the forwarded header
/// is preferred when the operator has declared they are behind one. It is
/// trusted only in that case — otherwise any client could spoof it and bypass
/// the limiter entirely.
pub fn client_key(peer: Option<IpAddr>, forwarded: Option<&str>, trust_forwarded: bool) -> String {
    if trust_forwarded {
        if let Some(value) = forwarded {
            // X-Forwarded-For is a chain; the left-most entry is the client.
            if let Some(first) = value.split(',').next() {
                let first = first.trim();
                if !first.is_empty() {
                    return first.to_string();
                }
            }
        }
    }
    peer.map(|ip| ip.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_allowed_then_refused() {
        let limiter = RateLimiter::new(5.0, 1.0);
        for i in 0..5 {
            assert!(limiter.check("1.2.3.4"), "request {i} should be allowed");
        }
        assert!(!limiter.check("1.2.3.4"), "the sixth should be refused");
    }

    #[test]
    fn keys_are_independent() {
        let limiter = RateLimiter::new(2.0, 1.0);
        assert!(limiter.check("a") && limiter.check("a"));
        assert!(!limiter.check("a"));
        assert!(
            limiter.check("b"),
            "one client must not exhaust another's allowance"
        );
    }

    #[test]
    fn allowance_refills_over_time() {
        let limiter = RateLimiter::new(1.0, 1000.0);
        assert!(limiter.check("x"));
        assert!(!limiter.check("x"));
        std::thread::sleep(Duration::from_millis(5));
        assert!(limiter.check("x"), "tokens should have refilled");
    }

    #[test]
    fn forwarded_header_is_used_only_when_trusted() {
        let peer = Some("10.0.0.1".parse().unwrap());
        // Untrusted: the header is ignored, so it cannot be used to evade limits.
        assert_eq!(client_key(peer, Some("1.2.3.4"), false), "10.0.0.1");
        // Trusted: the left-most entry of the chain identifies the client.
        assert_eq!(client_key(peer, Some("1.2.3.4, 10.0.0.9"), true), "1.2.3.4");
        assert_eq!(client_key(peer, None, true), "10.0.0.1");
        assert_eq!(client_key(peer, Some("   "), true), "10.0.0.1");
    }
}
