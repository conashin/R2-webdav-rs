use std::env;

use anyhow::{Context, Result};

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
}

fn required(key: &str) -> Result<String> {
    env::var(key).with_context(|| format!("missing required env var {key}"))
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
        })
    }
}
