use std::env;
use std::net::IpAddr;

use anyhow::{anyhow, Context, Result};

/// Validate `R2_PUBLIC_BASE_URL` to prevent SSRF. Only HTTPS is allowed, and
/// the host must not be a loopback, private, or link-local address, nor any of
/// the well-known cloud metadata endpoints. This runs at startup so an invalid
/// URL fails fast instead of producing redirects to internal targets.
fn validate_public_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(trimmed.to_string());
    }

    let parsed = url::Url::parse(trimmed)
        .with_context(|| format!("invalid R2_PUBLIC_BASE_URL: {trimmed}"))?;

    if parsed.scheme() != "https" {
        anyhow::bail!(
            "R2_PUBLIC_BASE_URL must use HTTPS (got `{}`)",
            parsed.scheme()
        );
    }
    if parsed.password().is_some() || parsed.username() != "" {
        anyhow::bail!("R2_PUBLIC_BASE_URL must not contain userinfo");
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("R2_PUBLIC_BASE_URL has no host"))?
        .trim_end_matches('.');

    // Block well-known metadata / internal hosts by name.
    const BLOCKED_HOSTS: &[&str] = &[
        "localhost",
        "metadata.google.internal",
        "metadata.azure.com",
        "metadata",
    ];
    if BLOCKED_HOSTS.iter().any(|b| host.eq_ignore_ascii_case(b)) {
        anyhow::bail!("R2_PUBLIC_BASE_URL host `{host}` is blocked");
    }

    // Block by IP literal: loopback, private, link-local, unspecified.
    if let Ok(ip) = host.parse::<IpAddr>() {
        let link_local = match ip {
            IpAddr::V4(v4) => v4.is_link_local(),
            IpAddr::V6(v6) => {
                let seg0 = v6.segments()[0];
                (seg0 & 0xffc0) == 0xfe80 // link-local fe80::/10
            }
        };
        if ip.is_loopback() || ip.is_unspecified() || link_local || is_private(ip) {
            anyhow::bail!("R2_PUBLIC_BASE_URL host `{host}` is a private/loopback IP");
        }
    }

    // Reject `169.254.169.254` and similar regardless of casing above.
    if host == "169.254.169.254" || host == "169.254.170.2" {
        anyhow::bail!("R2_PUBLIC_BASE_URL host `{host}` is a metadata endpoint");
    }

    Ok(trimmed.to_string())
}

/// `IpAddr::is_private()` only exists for IPv4 in std and covers RFC1918. We
/// reuse it for v4 and treat all non-global v6 as private for our purposes.
fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(v6) => {
            // Loopback, link-local, unique-local, unspecified all excluded above/below.
            v6.is_loopback() || {
                let seg0 = v6.segments()[0];
                (seg0 & 0xfe00) == 0xfc00 // unique local fc00::/7
            }
        }
    }
}

/// Runtime configuration, loaded entirely from environment variables.
#[derive(Clone, Debug)]
pub struct Config {
    /// Full S3 endpoint for the R2 bucket, e.g.
    /// `https://<account_id>.r2.cloudflarestorage.com`.
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub bucket: String,
    pub username: String,
    pub password: String,
    pub bind_addr: String,
    /// When `Some`, listen on a Unix domain socket at this path instead of
    /// (or in addition to) `bind_addr`. The socket file is removed at startup
    /// if present, and (when running as root or owner of the parent dir) is
    /// created with mode `0660` so a reverse proxy in a matching group can
    /// connect.
    pub bind_socket: Option<String>,
    /// When `true`, trust `X-Forwarded-For` for client-IP extraction. Only set
    /// when running behind a trusted reverse proxy that overwrites this header.
    pub trust_proxy: bool,
    /// Public base URL for the bucket (e.g. an r2.dev URL or a custom domain).
    /// When set, file `GET`s are answered with a `302` redirect to
    /// `<base>/<key>` instead of streaming through this server. `None` (unset
    /// or empty) disables the feature.
    pub public_base_url: Option<String>,
}

fn required(key: &str) -> Result<String> {
    let val = env::var(key).with_context(|| format!("missing required env var {key}"))?;
    // A present-but-empty value is almost always a misconfiguration (e.g. an
    // unexpanded shell/Compose variable, or an empty `.env` line). For the auth
    // credentials in particular, an empty value would silently weaken or fully
    // disable authentication, so fail closed at startup rather than boot with
    // broken auth. Length-0 only: a password of literal spaces is unusual but
    // legitimate, so we do not trim before checking.
    if val.is_empty() {
        anyhow::bail!("required env var {key} is set but empty");
    }
    Ok(val)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // Endpoint can be given directly, or derived from the account id.
        let endpoint = match env::var("R2_ENDPOINT") {
            Ok(ep) => ep,
            Err(_) => {
                let account_id =
                    required("R2_ACCOUNT_ID").context("set either R2_ENDPOINT or R2_ACCOUNT_ID")?;
                format!("https://{account_id}.r2.cloudflarestorage.com")
            }
        };

        Ok(Config {
            endpoint,
            access_key_id: required("R2_ACCESS_KEY_ID")?,
            secret_access_key: required("R2_SECRET_ACCESS_KEY")?,
            bucket: required("R2_BUCKET")?,
            username: required("WEBDAV_USERNAME")?,
            password: required("WEBDAV_PASSWORD")?,
            bind_addr: env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:4918".to_string()),
            bind_socket: env::var("BIND_SOCKET")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            trust_proxy: env::var("TRUST_PROXY").ok().as_deref() == Some("1"),
            public_base_url: env::var("R2_PUBLIC_BASE_URL")
                .ok()
                .map(|s| validate_public_base_url(&s))
                .transpose()?
                .filter(|s| !s.is_empty()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::required;
    use std::env;

    // Each test uses a unique env var name so the default parallel test runner
    // cannot race on shared process environment state.

    #[test]
    fn present_and_non_empty_is_ok() {
        let key = "R2_WEBDAV_TEST_REQUIRED_OK";
        env::set_var(key, "value");
        assert_eq!(required(key).unwrap(), "value");
        env::remove_var(key);
    }

    #[test]
    fn present_but_empty_is_rejected() {
        // Guards against the fail-open case: an empty credential must not boot.
        let key = "R2_WEBDAV_TEST_REQUIRED_EMPTY";
        env::set_var(key, "");
        assert!(required(key).is_err());
        env::remove_var(key);
    }

    #[test]
    fn missing_is_rejected() {
        let key = "R2_WEBDAV_TEST_REQUIRED_MISSING";
        env::remove_var(key);
        assert!(required(key).is_err());
    }

    #[test]
    fn whitespace_only_is_accepted() {
        // A password of literal spaces is unusual but legitimate; only length-0
        // is rejected, so this must succeed.
        let key = "R2_WEBDAV_TEST_REQUIRED_SPACES";
        env::set_var(key, "   ");
        assert_eq!(required(key).unwrap(), "   ");
        env::remove_var(key);
    }

    #[test]
    fn public_url_https_passes() {
        assert_eq!(
            super::validate_public_base_url("https://example.com/r2/").unwrap(),
            "https://example.com/r2"
        );
    }

    #[test]
    fn public_url_http_rejected() {
        assert!(super::validate_public_base_url("http://example.com").is_err());
    }

    #[test]
    fn public_url_loopback_rejected() {
        assert!(super::validate_public_base_url("https://127.0.0.1").is_err());
        assert!(super::validate_public_base_url("https://localhost").is_err());
    }

    #[test]
    fn public_url_metadata_rejected() {
        assert!(super::validate_public_base_url("https://169.254.169.254").is_err());
        assert!(super::validate_public_base_url("https://metadata.google.internal").is_err());
    }

    #[test]
    fn public_url_private_v4_rejected() {
        assert!(super::validate_public_base_url("https://10.0.0.1").is_err());
        assert!(super::validate_public_base_url("https://192.168.1.1").is_err());
        assert!(super::validate_public_base_url("https://172.16.0.1").is_err());
    }

    #[test]
    fn public_url_userinfo_rejected() {
        assert!(super::validate_public_base_url("https://user:pass@example.com").is_err());
    }
}
