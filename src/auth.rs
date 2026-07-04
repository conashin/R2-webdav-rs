//! HTTP Basic authentication.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hyper::header::AUTHORIZATION;
use hyper::HeaderMap;
use subtle::ConstantTimeEq;

use crate::config::Config;

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

    // Constant-time comparison to avoid leaking credential length/content
    // through timing.
    let user_ok = user.as_bytes().ct_eq(cfg.username.as_bytes());
    let pass_ok = pass.as_bytes().ct_eq(cfg.password.as_bytes());
    (user_ok & pass_ok).into()
}
