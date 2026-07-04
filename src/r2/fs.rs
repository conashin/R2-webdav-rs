//! `DavFileSystem` implementation mapping WebDAV operations onto R2 objects.

use std::sync::Arc;

use bytes::Bytes;
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};

use super::client::R2;
use super::file::R2File;
use super::meta::{to_system_time, R2DirEntry, R2MetaData};
use super::{dir_key, path_to_key};
use crate::config::Config;

#[derive(Clone)]
pub struct R2FileSystem {
    r2: Arc<R2>,
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
        }
    }

    /// Resolve directory metadata for a key, via an explicit `key/` marker or,
    /// failing that, the presence of any object under the prefix.
    async fn dir_metadata(&self, key: &str) -> FsResult<Box<dyn DavMetaData>> {
        let dk = dir_key(key);
        if let Ok(h) = self.r2.head(&dk).await {
            let modified = h.last_modified().map(to_system_time);
            return Ok(Box::new(R2MetaData {
                len: 0,
                modified,
                is_dir: true,
                etag: None,
            }));
        }
        let (files, dirs) = self.r2.list_dir(&dk).await?;
        if !files.is_empty() || !dirs.is_empty() {
            return Ok(Box::new(R2MetaData::dir()));
        }
        Err(FsError::NotFound)
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
                if options.create_new && self.r2.head(&key).await.is_ok() {
                    return Err(FsError::Exists);
                }
                Ok(Box::new(R2File::new_write(self.r2.clone(), key)) as Box<dyn DavFile>)
            } else {
                let head = self.r2.head(&key).await?;
                let size = head.content_length().unwrap_or(0).max(0) as u64;
                let modified = head.last_modified().map(to_system_time);
                let etag = head.e_tag().map(String::from);
                Ok(
                    Box::new(R2File::new_read(self.r2.clone(), key, size, modified, etag))
                        as Box<dyn DavFile>,
                )
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
            match self.r2.head(&key).await {
                Ok(h) => {
                    let size = h.content_length().unwrap_or(0).max(0) as u64;
                    let modified = h.last_modified().map(to_system_time);
                    let etag = h.e_tag().map(String::from);
                    Ok(Box::new(R2MetaData {
                        len: size,
                        modified,
                        is_dir: false,
                        etag,
                    }) as Box<dyn DavMetaData>)
                }
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
            if self.r2.head(&dk).await.is_ok() || self.r2.head(&key).await.is_ok() {
                return Err(FsError::Exists);
            }
            self.r2.put(&dk, Bytes::new()).await
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let key = path_to_key(path);
            // HEAD first so a missing object yields 404 rather than a silent 204.
            self.r2.head(&key).await?;
            self.r2.delete(&key).await
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let key = path_to_key(path);
            let dk = dir_key(&key);
            // Recursively delete everything under the prefix, marker included.
            let objs = self.r2.list_all(&dk).await?;
            for o in &objs {
                if let Some(k) = o.key() {
                    self.r2.delete(k).await?;
                }
            }
            let _ = self.r2.delete(&dk).await;
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
                return Ok(());
            }

            // Directory: move every object under the prefix.
            let fprefix = dir_key(&fk);
            let tprefix = dir_key(&tk);
            let objs = self.r2.list_all(&fprefix).await?;
            if objs.is_empty() {
                return Err(FsError::NotFound);
            }
            for o in &objs {
                if let Some(k) = o.key() {
                    let rel = k.strip_prefix(fprefix.as_str()).unwrap_or(k);
                    let newkey = format!("{tprefix}{rel}");
                    self.r2.copy(k, &newkey).await?;
                }
            }
            for o in &objs {
                if let Some(k) = o.key() {
                    self.r2.delete(k).await?;
                }
            }
            Ok(())
        })
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            let fk = path_to_key(from);
            let tk = path_to_key(to);

            if self.r2.head(&fk).await.is_ok() {
                return self.r2.copy(&fk, &tk).await;
            }

            let fprefix = dir_key(&fk);
            let tprefix = dir_key(&tk);
            let objs = self.r2.list_all(&fprefix).await?;
            if objs.is_empty() {
                return Err(FsError::NotFound);
            }
            for o in &objs {
                if let Some(k) = o.key() {
                    let rel = k.strip_prefix(fprefix.as_str()).unwrap_or(k);
                    let newkey = format!("{tprefix}{rel}");
                    self.r2.copy(k, &newkey).await?;
                }
            }
            Ok(())
        })
    }
}
