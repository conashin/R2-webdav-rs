//! `DavFileSystem` implementation mapping WebDAV operations onto R2 objects.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::stream::{self, StreamExt};

use super::client::R2;
use super::file::R2File;
use super::meta::{to_system_time, R2DirEntry, R2MetaData};
use super::{dir_key, path_to_key, public_url};
use crate::config::Config;

/// Max in-flight server-side copies when moving/copying a directory tree.
const COPY_CONCURRENCY: usize = 16;

/// How long a cached HEAD result stays fresh. WebDAV clients (and dav-server
/// itself) issue several `HeadObject`s for the same key within a single logical
/// operation — e.g. a `GET` heads once for `metadata` and again for `open`. A
/// short TTL collapses those duplicates while keeping staleness tightly bounded.
const HEAD_CACHE_TTL: Duration = Duration::from_secs(3);

/// Cap on cached HEAD entries; pruning kicks in past this to bound memory.
const HEAD_CACHE_CAP: usize = 10_000;

/// Distilled, cacheable result of a successful `HeadObject`. Only *positive*
/// hits are cached: not caching misses avoids the "upload then immediately read
/// returns 404" hazard, since a pre-upload `NotFound` is never remembered.
#[derive(Clone)]
struct HeadMeta {
    len: u64,
    modified: Option<SystemTime>,
    etag: Option<String>,
}

#[derive(Clone)]
pub struct R2FileSystem {
    r2: Arc<R2>,
    /// Public base URL for `GET` redirects; `None` disables redirecting.
    public_base: Option<Arc<str>>,
    /// Short-TTL cache of successful HEADs, keyed by object key. Shared across
    /// all connection tasks (one per accepted socket).
    head_cache: Arc<Mutex<HashMap<String, (Instant, HeadMeta)>>>,
}

impl std::fmt::Debug for R2FileSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("R2FileSystem")
    }
}

impl R2FileSystem {
    pub fn new(cfg: &Config) -> Self {
        R2FileSystem {
            r2: Arc::new(R2::new(cfg)),
            public_base: cfg.public_base_url.as_deref().map(Arc::from),
            head_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// HEAD `key`, serving a fresh cached result when one exists. Misses and
    /// non-`NotFound` errors are never cached. Read-only metadata paths use this;
    /// mutating paths call `self.r2.head` directly and invalidate afterwards.
    async fn cached_head(&self, key: &str) -> FsResult<HeadMeta> {
        if let Some(m) = self.head_cache_get(key) {
            return Ok(m);
        }
        let h = self.r2.head(key).await?;
        let m = HeadMeta {
            len: h.content_length().unwrap_or(0).max(0) as u64,
            modified: h.last_modified().map(to_system_time),
            etag: h.e_tag().map(String::from),
        };
        self.head_cache_put(key, m.clone());
        Ok(m)
    }

    fn head_cache_get(&self, key: &str) -> Option<HeadMeta> {
        let map = self.head_cache.lock().unwrap();
        let (at, m) = map.get(key)?;
        (at.elapsed() < HEAD_CACHE_TTL).then(|| m.clone())
    }

    fn head_cache_put(&self, key: &str, meta: HeadMeta) {
        let mut map = self.head_cache.lock().unwrap();
        if map.len() >= HEAD_CACHE_CAP {
            // Drop stale entries first; if still full, clear wholesale.
            map.retain(|_, (at, _)| at.elapsed() < HEAD_CACHE_TTL);
            if map.len() >= HEAD_CACHE_CAP {
                map.clear();
            }
        }
        map.insert(key.to_string(), (Instant::now(), meta));
    }

    fn head_cache_invalidate(&self, key: &str) {
        self.head_cache.lock().unwrap().remove(key);
    }

    fn head_cache_clear(&self) {
        self.head_cache.lock().unwrap().clear();
    }

    /// Resolve directory metadata for a key, via an explicit `key/` marker or,
    /// failing that, the presence of any object under the prefix.
    async fn dir_metadata(&self, key: &str) -> FsResult<Box<dyn DavMetaData>> {
        let dk = dir_key(key);
        // Probe the explicit marker and list the prefix concurrently; the marker
        // gives us an mtime, the listing catches marker-less directories.
        let (head, listing) = tokio::join!(self.cached_head(&dk), self.r2.list_dir(&dk));
        if let Ok(h) = head {
            return Ok(Box::new(R2MetaData {
                len: 0,
                modified: h.modified,
                is_dir: true,
                etag: None,
            }));
        }
        let (files, dirs) = listing?;
        if !files.is_empty() || !dirs.is_empty() {
            return Ok(Box::new(R2MetaData::dir()));
        }
        Err(FsError::NotFound)
    }

    /// Server-side copy every `keys` entry from under `fprefix` to `tprefix`,
    /// with bounded concurrency. Returns the first error encountered.
    async fn copy_keys(&self, keys: &[String], fprefix: &str, tprefix: &str) -> FsResult<()> {
        let pairs: Vec<(String, String)> = keys
            .iter()
            .map(|k| {
                let rel = k.strip_prefix(fprefix).unwrap_or(k);
                (k.clone(), format!("{tprefix}{rel}"))
            })
            .collect();
        stream::iter(pairs)
            .map(|(src, dst)| async move { self.r2.copy(&src, &dst).await })
            .buffer_unordered(COPY_CONCURRENCY)
            .collect::<Vec<FsResult<()>>>()
            .await
            .into_iter()
            .collect()
    }
}

impl DavFileSystem for R2FileSystem {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            let key = path_to_key(path);
            if key.is_empty() || key.ends_with('/') {
                return Err(FsError::Forbidden);
            }

            if options.write {
                // Existence check must be authoritative, so read through the
                // cache. Drop any cached entry: this key's object is about to
                // change once the upload completes.
                if options.create_new && self.r2.head(&key).await.is_ok() {
                    return Err(FsError::Exists);
                }
                self.head_cache_invalidate(&key);
                Ok(Box::new(R2File::new_write(self.r2.clone(), key)) as Box<dyn DavFile>)
            } else {
                let head = self.cached_head(&key).await?;
                let redirect = self.public_base.as_ref().map(|base| public_url(base, &key));
                Ok(Box::new(R2File::new_read(
                    self.r2.clone(),
                    key,
                    head.len,
                    head.modified,
                    head.etag,
                    redirect,
                )) as Box<dyn DavFile>)
            }
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            let key = path_to_key(path);
            if key.is_empty() {
                return Ok(Box::new(R2MetaData::dir()) as Box<dyn DavMetaData>);
            }
            if key.ends_with('/') {
                return self.dir_metadata(&key).await;
            }
            match self.cached_head(&key).await {
                Ok(h) => Ok(Box::new(R2MetaData {
                    len: h.len,
                    modified: h.modified,
                    is_dir: false,
                    etag: h.etag,
                }) as Box<dyn DavMetaData>),
                Err(FsError::NotFound) => self.dir_metadata(&key).await,
                Err(e) => Err(e),
            }
        })
    }

    fn symlink_metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        // R2 has no symlinks; identical to metadata.
        self.metadata(path)
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            let key = path_to_key(path);
            let prefix = dir_key(&key);
            let (files, dirs) = self.r2.list_dir(&prefix).await?;

            let mut entries: Vec<FsResult<Box<dyn DavDirEntry>>> = Vec::new();

            for cp in dirs {
                if let Some(p) = cp.prefix() {
                    let name = p
                        .strip_prefix(prefix.as_str())
                        .unwrap_or(p)
                        .trim_end_matches('/');
                    if name.is_empty() {
                        continue;
                    }
                    entries.push(Ok(Box::new(R2DirEntry {
                        name: name.as_bytes().to_vec(),
                        meta: R2MetaData::dir(),
                    })));
                }
            }

            for obj in files {
                let k = obj.key().unwrap_or_default();
                // Skip the directory's own marker object.
                if k == prefix {
                    continue;
                }
                let name = k.strip_prefix(prefix.as_str()).unwrap_or(k);
                if name.is_empty() || name.ends_with('/') {
                    continue;
                }
                let meta = R2MetaData {
                    len: obj.size().unwrap_or(0).max(0) as u64,
                    modified: obj.last_modified().map(to_system_time),
                    is_dir: false,
                    etag: obj.e_tag().map(String::from),
                };
                entries.push(Ok(Box::new(R2DirEntry {
                    name: name.as_bytes().to_vec(),
                    meta,
                })));
            }

            let stream = futures_util::stream::iter(entries);
            Ok(Box::pin(stream) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let key = path_to_key(path);
            let dk = dir_key(&key);
            if dk.is_empty() {
                return Err(FsError::Forbidden);
            }
            // Fail if a directory or a file already exists at this path.
            let (dir_exists, file_exists) = tokio::join!(self.r2.head(&dk), self.r2.head(&key));
            if dir_exists.is_ok() || file_exists.is_ok() {
                return Err(FsError::Exists);
            }
            self.r2.put(&dk, Bytes::new()).await?;
            self.head_cache_invalidate(&dk);
            Ok(())
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let key = path_to_key(path);
            // HEAD first so a missing object yields 404 rather than a silent 204.
            self.r2.head(&key).await?;
            self.r2.delete(&key).await?;
            self.head_cache_invalidate(&key);
            Ok(())
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let key = path_to_key(path);
            let dk = dir_key(&key);
            // Recursively delete everything under the prefix, marker included,
            // in batched DeleteObjects requests (up to 1000 keys each).
            let objs = self.r2.list_all(&dk).await?;
            let mut keys: Vec<String> = objs
                .iter()
                .filter_map(|o| o.key().map(String::from))
                .collect();
            if !keys.iter().any(|k| k == &dk) {
                keys.push(dk);
            }
            self.r2.delete_many(&keys).await?;
            // A whole subtree changed; drop the lot rather than track each key.
            self.head_cache_clear();
            Ok(())
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let fk = path_to_key(from);
            let tk = path_to_key(to);

            // Single file: copy then delete the source.
            if self.r2.head(&fk).await.is_ok() {
                self.r2.copy(&fk, &tk).await?;
                self.r2.delete(&fk).await?;
                self.head_cache_invalidate(&fk);
                self.head_cache_invalidate(&tk);
                return Ok(());
            }

            // Directory: move every object under the prefix.
            let fprefix = dir_key(&fk);
            let tprefix = dir_key(&tk);
            let objs = self.r2.list_all(&fprefix).await?;
            if objs.is_empty() {
                return Err(FsError::NotFound);
            }
            let keys: Vec<String> = objs
                .iter()
                .filter_map(|o| o.key().map(String::from))
                .collect();
            self.copy_keys(&keys, &fprefix, &tprefix).await?;
            self.r2.delete_many(&keys).await?;
            // Both subtrees changed; drop the lot rather than track each key.
            self.head_cache_clear();
            Ok(())
        })
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let fk = path_to_key(from);
            let tk = path_to_key(to);

            if self.r2.head(&fk).await.is_ok() {
                self.r2.copy(&fk, &tk).await?;
                self.head_cache_invalidate(&tk);
                return Ok(());
            }

            let fprefix = dir_key(&fk);
            let tprefix = dir_key(&tk);
            let objs = self.r2.list_all(&fprefix).await?;
            if objs.is_empty() {
                return Err(FsError::NotFound);
            }
            let keys: Vec<String> = objs
                .iter()
                .filter_map(|o| o.key().map(String::from))
                .collect();
            self.copy_keys(&keys, &fprefix, &tprefix).await?;
            // The destination subtree changed; drop the lot.
            self.head_cache_clear();
            Ok(())
        })
    }
}
