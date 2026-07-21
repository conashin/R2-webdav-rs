//! Cloudflare R2 backend for the WebDAV filesystem.
//!
//! R2 has no notion of directories. We model a directory as a zero-byte marker
//! object whose key ends in `/` (e.g. `photos/`), and list with `delimiter="/"`
//! so `CommonPrefixes` map to sub-directories and `Contents` to files.

mod client;
mod file;
mod fs;
mod meta;

pub use fs::R2FileSystem;

use aws_sdk_s3::error::SdkError;
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use dav_server::davpath::DavPath;
use dav_server::fs::FsError;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// Characters escaped when embedding an object key in a public URL path.
/// `/` is left intact so it keeps separating path segments.
const PUBLIC_URL_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'|')
    .add(b'\\')
    .add(b'^');

/// Map an AWS SDK error onto the WebDAV filesystem error type.
///
/// The concrete per-operation error enums all share the same transport
/// response type, so we classify by HTTP status where possible.
pub(crate) fn to_fs_err<E: std::fmt::Debug>(err: SdkError<E, HttpResponse>) -> FsError {
    let status = err.raw_response().map(|r| r.status().as_u16());
    match status {
        Some(404) => FsError::NotFound,
        Some(403) => FsError::Forbidden,
        Some(412) => FsError::Exists,
        _ => {
            // Avoid logging the full SdkError Debug representation, which may
            // include request/response bodies, signatures, or headers. Log
            // only the status code and the Display form of the inner error if
            // available, falling back to the error's type name.
            let kind = std::any::type_name::<E>();
            let msg = std::format!("{kind} ");
            tracing::error!(status = status, error_kind = %msg, "R2 request failed");
            FsError::GeneralFailure
        }
    }
}

/// Convert a WebDAV request path into an R2 object key (no leading slash).
pub(crate) fn path_to_key(path: &DavPath) -> String {
    let raw = path.as_bytes();
    String::from_utf8_lossy(raw)
        .trim_start_matches('/')
        .to_string()
}

/// Ensure a key has a trailing slash so it names a "directory" prefix.
/// The empty (root) key is returned unchanged.
pub(crate) fn dir_key(key: &str) -> String {
    if key.is_empty() || key.ends_with('/') {
        key.to_string()
    } else {
        format!("{key}/")
    }
}

/// Build the public URL for an object key under `base`, percent-encoding the
/// key while preserving `/` path separators. `base` has no trailing slash.
pub(crate) fn public_url(base: &str, key: &str) -> String {
    format!("{base}/{}", utf8_percent_encode(key, PUBLIC_URL_SET))
}
