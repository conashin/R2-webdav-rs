//! Metadata and directory-entry types.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_sdk_s3::primitives::DateTime;
use dav_server::fs::{DavDirEntry, DavMetaData, FsFuture, FsResult};

/// Convert an S3 `DateTime` to a `SystemTime`.
pub(crate) fn to_system_time(dt: &DateTime) -> SystemTime {
    let secs = dt.secs();
    let nanos = dt.subsec_nanos();
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nanos)
    } else {
        UNIX_EPOCH - Duration::new((-secs) as u64, nanos)
    }
}

#[derive(Clone, Debug)]
pub struct R2MetaData {
    pub len: u64,
    pub modified: Option<SystemTime>,
    pub is_dir: bool,
    pub etag: Option<String>,
}

impl R2MetaData {
    pub fn dir() -> Self {
        R2MetaData {
            len: 0,
            modified: None,
            is_dir: true,
            etag: None,
        }
    }
}

impl DavMetaData for R2MetaData {
    fn len(&self) -> u64 {
        self.len
    }

    fn modified(&self) -> FsResult<SystemTime> {
        self.modified.ok_or(dav_server::fs::FsError::NotImplemented)
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }

    fn etag(&self) -> Option<String> {
        // R2/S3 etags already come quoted; strip the quotes for cleanliness.
        self.etag.as_ref().map(|e| e.trim_matches('"').to_string())
    }
}

/// A single entry returned from `read_dir`.
pub struct R2DirEntry {
    pub name: Vec<u8>,
    pub meta: R2MetaData,
}

impl DavDirEntry for R2DirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        Box::pin(async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) })
    }
}
