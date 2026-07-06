//! HTTP Basic authentication.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hyper::header::AUTHORIZATION;
use hyper::HeaderMap;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::Config;

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
    use super::check;
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
}
