//! HTTP Basic authentication with per-IP rate limiting and lockout.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hyper::header::AUTHORIZATION;
use hyper::HeaderMap;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::Config;

/// Maximum authentication attempts per IP within the window.
const MAX_ATTEMPTS: u32 = 10;
/// Sliding window in which `MAX_ATTEMPTS` failures are tolerated.
const WINDOW: Duration = Duration::from_secs(60);
/// Lockout duration after exceeding the threshold.
const LOCKOUT: Duration = Duration::from_secs(300);
/// Prune interval for the failure map so it does not grow unbounded.
const PRUNE_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
enum State {
    /// `n` failures since `at`, not yet over threshold.
    Counting { n: u32, at: Instant },
    /// Locked out until `until`.
    Locked { until: Instant },
}

/// Per-IP rate limiter shared across all connection tasks.
pub struct RateLimiter {
    map: Mutex<HashMap<String, State>>,
    last_prune: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new() -> Self {
        RateLimiter {
            map: Mutex::new(HashMap::new()),
            last_prune: Mutex::new(Instant::now()),
        }
    }

    /// Returns `true` if the IP may attempt authentication, `false` if locked.
    pub fn check(&self, ip: &str) -> bool {
        let mut map = self.map.lock().unwrap();
        let now = Instant::now();
        self.maybe_prune(&mut map, now);
        match map.get(ip) {
            Some(State::Locked { until }) => {
                if now < *until {
                    return false;
                }
                // Lockout expired: reset and allow.
                map.remove(ip);
                true
            }
            Some(State::Counting { n, at }) => {
                // Window expired: reset silently.
                if now.duration_since(*at) > WINDOW {
                    map.remove(ip);
                } else if *n >= MAX_ATTEMPTS {
                    return false;
                }
                true
            }
            None => true,
        }
    }

    /// Record a failed attempt; transitions to locked when threshold is hit.
    pub fn record_failure(&self, ip: &str) {
        let mut map = self.map.lock().unwrap();
        let now = Instant::now();
        let entry = map
            .entry(ip.to_string())
            .or_insert(State::Counting { n: 0, at: now });
        match entry {
            State::Counting { n, at } => {
                if now.duration_since(*at) > WINDOW {
                    *n = 1;
                    *at = now;
                } else {
                    *n = n.saturating_add(1);
                }
                if *n >= MAX_ATTEMPTS {
                    *entry = State::Locked {
                        until: now + LOCKOUT,
                    };
                }
            }
            State::Locked { until } => {
                if now >= *until {
                    *entry = State::Counting { n: 1, at: now };
                }
            }
        }
    }

    /// Clear failures on a successful auth so the window does not accumulate.
    pub fn record_success(&self, ip: &str) {
        self.map.lock().unwrap().remove(ip);
    }

    fn maybe_prune(&self, map: &mut HashMap<String, State>, now: Instant) {
        let mut last = self.last_prune.lock().unwrap();
        if now.duration_since(*last) < PRUNE_EVERY {
            return;
        }
        *last = now;
        map.retain(|_, s| match s {
            State::Counting { at, .. } => now.duration_since(*at) < WINDOW,
            State::Locked { until } => now < *until,
        });
    }
}

/// Header name for `X-Forwarded-For`, defined manually because `hyper::header`
/// does not expose it as a constant.
pub const X_FORWARDED_FOR: &str = "x-forwarded-for";

/// Extract a client IP from headers, falling back to the literal peer.
/// `X-Forwarded-For` is trusted only when `trust_proxy` is set (callers MUST
/// ensure the proxy overwrites this header at the network edge).
pub fn client_ip(headers: &HeaderMap, peer: &str, trust_proxy: bool) -> String {
    if trust_proxy {
        if let Some(xff) = headers.get(X_FORWARDED_FOR) {
            if let Ok(s) = xff.to_str() {
                if let Some(first) = s.split(',').next() {
                    let trimmed = first.trim();
                    if !trimmed.is_empty() {
                        return trimmed.to_string();
                    }
                }
            }
        }
    }
    peer.to_string()
}

/// SHA-256 of the input. Comparing digests instead of the raw bytes keeps the
/// comparison a fixed 32 bytes, so `ConstantTimeEq` never short-circuits and the
/// *length* of the configured credential cannot leak via timing.
fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Returns `true` if the request carries valid Basic credentials.
pub fn check(headers: &HeaderMap, cfg: &Config) -> bool {
    let Some(value) = headers.get(AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(encoded) = value.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return false;
    };
    let Some((user, pass)) = decoded.split_once(':') else {
        return false;
    };

    // Compare fixed-length SHA-256 digests in constant time. `subtle`'s slice
    // `ct_eq` short-circuits on a length mismatch, which would leak the length
    // of the configured username/password through timing; hashing to a constant
    // 32 bytes first closes that side channel. Both digests are always computed
    // and both comparisons run, so neither length nor content leaks.
    let user_ok = digest(user.as_bytes()).ct_eq(&digest(cfg.username.as_bytes()));
    let pass_ok = digest(pass.as_bytes()).ct_eq(&digest(cfg.password.as_bytes()));
    (user_ok & pass_ok).into()
}

#[cfg(test)]
mod tests {
    use super::{check, X_FORWARDED_FOR};
    use crate::config::Config;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use hyper::header::AUTHORIZATION;
    use hyper::HeaderMap;

    fn test_config() -> Config {
        Config {
            endpoint: "https://example.r2.cloudflarestorage.com".to_string(),
            access_key_id: "ak".to_string(),
            secret_access_key: "sk".to_string(),
            bucket: "bucket".to_string(),
            username: "alice".to_string(),
            password: "s3cret".to_string(),
            bind_addr: "0.0.0.0:4918".to_string(),
            bind_socket: None,
            trust_proxy: false,
            public_base_url: None,
        }
    }

    fn headers_with_basic(credentials: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        let value = format!("Basic {}", STANDARD.encode(credentials));
        h.insert(AUTHORIZATION, value.parse().unwrap());
        h
    }

    #[test]
    fn accepts_correct_credentials() {
        assert!(check(&headers_with_basic("alice:s3cret"), &test_config()));
    }

    #[test]
    fn rejects_wrong_value_same_length() {
        // Same lengths as the real creds, wrong bytes: exercises the digest
        // comparison rather than any length short-circuit.
        assert!(!check(&headers_with_basic("bobby:X3cret"), &test_config()));
    }

    #[test]
    fn rejects_wrong_length_credentials() {
        // The case that previously leaked length via timing.
        assert!(!check(&headers_with_basic("a:b"), &test_config()));
        assert!(!check(
            &headers_with_basic("alice_longer:s3cret_longer"),
            &test_config()
        ));
    }

    #[test]
    fn rejects_wrong_password_only() {
        assert!(!check(&headers_with_basic("alice:wrong"), &test_config()));
    }

    #[test]
    fn rejects_missing_and_malformed_headers() {
        let cfg = test_config();
        // No Authorization header.
        assert!(!check(&HeaderMap::new(), &cfg));
        // Non-Basic scheme.
        let mut bearer = HeaderMap::new();
        bearer.insert(AUTHORIZATION, "Bearer token".parse().unwrap());
        assert!(!check(&bearer, &cfg));
        // "Basic" but not valid base64.
        let mut bad_b64 = HeaderMap::new();
        bad_b64.insert(AUTHORIZATION, "Basic not@base64".parse().unwrap());
        assert!(!check(&bad_b64, &cfg));
        // Valid base64 but no ':' separator.
        assert!(!check(&headers_with_basic("nocolon"), &cfg));
    }

    #[test]
    fn rate_limiter_allows_under_threshold() {
        let rl = super::RateLimiter::new();
        for _ in 0..super::MAX_ATTEMPTS - 1 {
            assert!(rl.check("1.2.3.4"));
            rl.record_failure("1.2.3.4");
        }
        assert!(rl.check("1.2.3.4"));
    }

    #[test]
    fn rate_limiter_locks_after_threshold() {
        let rl = super::RateLimiter::new();
        for _ in 0..super::MAX_ATTEMPTS {
            rl.record_failure("1.2.3.4");
        }
        // The next check should be locked.
        assert!(!rl.check("1.2.3.4"));
    }

    #[test]
    fn rate_limiter_success_resets() {
        let rl = super::RateLimiter::new();
        rl.record_failure("1.2.3.4");
        rl.record_success("1.2.3.4");
        // After success the window was cleared.
        assert!(rl.check("1.2.3.4"));
    }

    #[test]
    fn client_ip_uses_xff_only_when_trusted() {
        let mut h = HeaderMap::new();
        h.insert(X_FORWARDED_FOR, "9.9.9.9, 8.8.8.8".parse().unwrap());
        assert_eq!(super::client_ip(&h, "1.1.1.1", true), "9.9.9.9");
        assert_eq!(super::client_ip(&h, "1.1.1.1", false), "1.1.1.1");
    }
}
