//! `DavFile` implementation: streaming reads via ranged GET, streaming writes
//! via S3 multipart upload so memory stays bounded regardless of file size.

use std::io::SeekFrom;
use std::sync::Arc;
use std::time::SystemTime;

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::CompletedPart;
use bytes::{Buf, Bytes, BytesMut};
use dav_server::fs::{DavFile, DavMetaData, FsError, FsFuture, FsResult};

use super::client::R2;
use super::meta::R2MetaData;

/// Part size for multipart uploads. R2's minimum part size is 5 MiB (all but
/// the last part); 8 MiB gives headroom and keeps request count reasonable.
const PART_SIZE: usize = 8 * 1024 * 1024;

impl std::fmt::Debug for R2File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("R2File")
            .field("key", &self.key)
            .field("is_write", &self.is_write)
            .finish()
    }
}

pub struct R2File {
    r2: Arc<R2>,
    key: String,
    is_write: bool,

    // --- read state ---
    pos: u64,
    size: u64,
    modified: Option<SystemTime>,
    etag: Option<String>,
    stream: Option<ByteStream>,
    read_buf: Bytes,
    /// If set, GET is answered with a 302 redirect to this URL (handler must be
    /// built with `.redirect(true)`).
    redirect: Option<String>,

    // --- write state ---
    write_buf: BytesMut,
    upload_id: Option<String>,
    parts: Vec<CompletedPart>,
    written: u64,
}

impl R2File {
    pub fn new_read(
        r2: Arc<R2>,
        key: String,
        size: u64,
        modified: Option<SystemTime>,
        etag: Option<String>,
        redirect: Option<String>,
    ) -> Self {
        R2File {
            r2,
            key,
            is_write: false,
            pos: 0,
            size,
            modified,
            etag,
            stream: None,
            read_buf: Bytes::new(),
            redirect,
            write_buf: BytesMut::new(),
            upload_id: None,
            parts: Vec::new(),
            written: 0,
        }
    }

    pub fn new_write(r2: Arc<R2>, key: String) -> Self {
        R2File {
            r2,
            key,
            is_write: true,
            pos: 0,
            size: 0,
            modified: None,
            etag: None,
            stream: None,
            read_buf: Bytes::new(),
            redirect: None,
            write_buf: BytesMut::new(),
            upload_id: None,
            parts: Vec::new(),
            written: 0,
        }
    }

    /// Upload the buffered data as one multipart part, starting the upload on
    /// first use.
    async fn flush_part(&mut self, data: Bytes) -> FsResult<()> {
        if self.upload_id.is_none() {
            self.upload_id = Some(self.r2.create_multipart(&self.key).await?);
        }
        let upload_id = self.upload_id.as_ref().unwrap();
        let part_number = self.parts.len() as i32 + 1;
        let part = self
            .r2
            .upload_part(&self.key, upload_id, part_number, data)
            .await?;
        self.parts.push(part);
        Ok(())
    }
}

impl DavFile for R2File {
    fn metadata(&mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = if self.is_write {
            R2MetaData {
                len: self.written,
                modified: Some(SystemTime::now()),
                is_dir: false,
                etag: None,
            }
        } else {
            R2MetaData {
                len: self.size,
                modified: self.modified,
                is_dir: false,
                etag: self.etag.clone(),
            }
        };
        Box::pin(async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) })
    }

    fn write_buf(&mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let bytes = buf.copy_to_bytes(buf.remaining());
            self.write_bytes(bytes).await
        })
    }

    fn write_bytes(&mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            self.written += buf.len() as u64;
            self.write_buf.extend_from_slice(&buf);
            while self.write_buf.len() >= PART_SIZE {
                let part = self.write_buf.split_to(PART_SIZE).freeze();
                self.flush_part(part).await?;
            }
            Ok(())
        })
    }

    fn read_bytes(&mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            if self.read_buf.is_empty() {
                if self.stream.is_none() {
                    if self.pos >= self.size {
                        return Ok(Bytes::new());
                    }
                    self.stream = Some(self.r2.get_range(&self.key, self.pos).await?);
                }
                let stream = self.stream.as_mut().unwrap();
                match stream.next().await {
                    Some(Ok(chunk)) => self.read_buf = chunk,
                    Some(Err(e)) => {
                        tracing::error!(error = ?e, "R2 stream read failed");
                        return Err(FsError::GeneralFailure);
                    }
                    None => return Ok(Bytes::new()),
                }
            }
            let n = count.min(self.read_buf.len());
            let out = self.read_buf.split_to(n);
            self.pos += n as u64;
            Ok(out)
        })
    }

    fn seek(&mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let new_pos = match pos {
                SeekFrom::Start(n) => n,
                SeekFrom::Current(n) => (self.pos as i64 + n).max(0) as u64,
                SeekFrom::End(n) => (self.size as i64 + n).max(0) as u64,
            };
            self.pos = new_pos;
            // Force the next read to re-open the stream at the new offset.
            self.stream = None;
            self.read_buf = Bytes::new();
            Ok(new_pos)
        })
    }

    fn flush(&mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            if !self.is_write {
                return Ok(());
            }
            if self.upload_id.is_some() {
                if !self.write_buf.is_empty() {
                    let part = std::mem::take(&mut self.write_buf).freeze();
                    self.flush_part(part).await?;
                }
                let upload_id = self.upload_id.clone().unwrap();
                let parts = std::mem::take(&mut self.parts);
                self.r2
                    .complete_multipart(&self.key, &upload_id, parts)
                    .await?;
                // Mark complete so Drop does not abort a finished upload.
                self.upload_id = None;
            } else {
                // Small file that never reached a full part: single PUT.
                let body = std::mem::take(&mut self.write_buf).freeze();
                self.r2.put(&self.key, body).await?;
            }
            Ok(())
        })
    }

    fn redirect_url(&mut self) -> FsFuture<'_, Option<String>> {
        let url = self.redirect.clone();
        Box::pin(async move { Ok(url) })
    }
}

impl Drop for R2File {
    fn drop(&mut self) {
        // Abort any multipart upload that was started but never completed
        // (e.g. the client disconnected mid-PUT) to avoid orphaned parts.
        if let Some(upload_id) = self.upload_id.take() {
            let r2 = self.r2.clone();
            let key = self.key.clone();
            tokio::spawn(async move {
                let _ = r2.abort_multipart(&key, &upload_id).await;
            });
        }
    }
}
