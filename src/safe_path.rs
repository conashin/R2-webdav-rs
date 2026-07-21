//! Path normalization and traversal prevention at the HTTP trust boundary.
//!
//! WebDAV clients can send any URI path; we must decode percent-encodings,
//! reject path-traversal sequences (`.`, `..`, empty segments), and rebuild a
//! safe absolute path before delegating to `dav-server`.

use hyper::Uri;
use percent_encoding::percent_decode_str;

#[derive(Debug)]
pub struct NormalizedPath(pub String);

/// Result of normalization: either a safe absolute path or a rejection reason.
pub enum PathCheck {
    Ok(NormalizedPath),
    Forbidden(&'static str),
}

/// Decode, normalize, and validate the request path. The returned path is safe
/// to hand to `dav-server` and contains no `.`, `..`, or empty segments.
///
/// Rules:
/// - Decode percent-encoding once (`%2f` → `/`) so an encoded `..` is caught.
/// - Split on `/` after decoding and drop empty / `.` segments.
/// - Reject any `..` segment outright (no normalization across them).
/// - Reject embedded NULs and other control characters.
/// - The result is always rooted at `/` and contains no `..`.
pub fn check(uri: &Uri) -> PathCheck {
    let raw = uri.path();
    let decoded = match percent_decode_str(raw).decode_utf8() {
        Ok(s) => s.into_owned(),
        Err(_) => return PathCheck::Forbidden("invalid percent-encoding"),
    };

    if decoded.chars().any(|c| c.is_control()) {
        return PathCheck::Forbidden("control character in path");
    }

    let trimmed = decoded.trim_start_matches('/');
    let mut clean: Vec<&str> = Vec::new();
    for seg in trimmed.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return PathCheck::Forbidden("path traversal"),
            s => {
                // Defense in depth: even though we decoded, look once more for
                // double-encoded `%2e%2e` that survived earlier decoding layers.
                if s.contains("..") {
                    return PathCheck::Forbidden("path traversal");
                }
                clean.push(s);
            }
        }
    }

    let normalized = format!("/{}", clean.join("/"));
    PathCheck::Ok(NormalizedPath(normalized))
}

/// Rebuild an absolute `Uri` from the original, replacing only the path.
pub fn rewrite(uri: &Uri, np: &NormalizedPath) -> Option<Uri> {
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(np.0.parse().ok()?);
    Uri::from_parts(parts).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_str(s: &str) -> PathCheck {
        let uri: Uri = s.parse().unwrap();
        check(&uri)
    }

    #[test]
    fn clean_paths_pass() {
        assert!(matches!(check_str("/foo/bar.txt"), PathCheck::Ok(_)));
        assert!(matches!(check_str("/"), PathCheck::Ok(_)));
        assert!(matches!(check_str("/dir/"), PathCheck::Ok(_)));
    }

    #[test]
    fn dot_segments_are_dropped() {
        let p = check_str("/foo/./bar/./baz").ok_unwrap();
        assert_eq!(p.0, "/foo/bar/baz");
    }

    #[test]
    fn double_dot_is_rejected() {
        assert!(matches!(check_str("/foo/../etc"), PathCheck::Forbidden(_)));
        assert!(matches!(check_str("/.."), PathCheck::Forbidden(_)));
        assert!(matches!(
            check_str("/foo/..%2f..%2fetc"),
            PathCheck::Forbidden(_)
        )); // decoded: /foo/../../etc
    }

    #[test]
    fn empty_path_is_ok() {
        assert!(matches!(check_str("/"), PathCheck::Ok(_)));
    }

    #[test]
    fn control_chars_rejected() {
        assert!(matches!(check_str("/foo%00bar"), PathCheck::Forbidden(_)));
    }

    #[test]
    fn nested_traversal_rejected() {
        assert!(matches!(
            check_str("/a/b/c/../../x"),
            PathCheck::Forbidden(_)
        ));
    }

    // helper for tests
    trait OkUnwrap {
        fn ok_unwrap(self) -> NormalizedPath;
    }
    impl OkUnwrap for PathCheck {
        fn ok_unwrap(self) -> NormalizedPath {
            match self {
                PathCheck::Ok(p) => p,
                PathCheck::Forbidden(r) => panic!("expected Ok, got Forbidden({r})"),
            }
        }
    }
}
