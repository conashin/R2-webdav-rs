//! Thin wrapper around the `aws-sdk-s3` client, configured for Cloudflare R2.

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{
    Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation,
};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    CommonPrefix, CompletedMultipartUpload, CompletedPart, Delete, Object, ObjectIdentifier,
};
use aws_sdk_s3::Client;
use bytes::Bytes;
use dav_server::fs::FsResult;
use futures_util::stream::{self, StreamExt};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

use super::to_fs_err;
use crate::config::Config;

/// Max in-flight `DeleteObjects` requests when deleting a large tree.
const DELETE_CONCURRENCY: usize = 16;

/// Characters that must be escaped in an `x-amz-copy-source` header value.
/// `/` is deliberately left unescaped so it keeps separating path segments.
const COPY_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'?')
    .add(b'+');

/// An R2 bucket handle plus the underlying S3 client.
#[derive(Clone)]
pub struct R2 {
    client: Client,
    bucket: String,
}

impl R2 {
    pub fn new(cfg: &Config) -> Self {
        let creds = Credentials::new(
            cfg.access_key_id.clone(),
            cfg.secret_access_key.clone(),
            None,
            None,
            "r2-webdav",
        );
        let s3_config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("auto"))
            .endpoint_url(cfg.endpoint.clone())
            .credentials_provider(creds)
            .force_path_style(true)
            // R2 rejects the SDK's default "when supported" checksum headers.
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .build();

        R2 {
            client: Client::from_conf(s3_config),
            bucket: cfg.bucket.clone(),
        }
    }

    /// HEAD an object; `FsError::NotFound` if it does not exist.
    pub async fn head(
        &self,
        key: &str,
    ) -> FsResult<aws_sdk_s3::operation::head_object::HeadObjectOutput> {
        self.client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(to_fs_err)
    }

    /// List a single directory level: `(files, sub-directory prefixes)`.
    pub async fn list_dir(&self, prefix: &str) -> FsResult<(Vec<Object>, Vec<CommonPrefix>)> {
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let resp = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .delimiter("/")
                .set_continuation_token(token)
                .send()
                .await
                .map_err(to_fs_err)?;
            files.extend(resp.contents().iter().cloned());
            dirs.extend(resp.common_prefixes().iter().cloned());
            if resp.is_truncated().unwrap_or(false) {
                token = resp.next_continuation_token().map(String::from);
            } else {
                break;
            }
        }
        Ok((files, dirs))
    }

    /// List every object under `prefix` (recursive, no delimiter).
    pub async fn list_all(&self, prefix: &str) -> FsResult<Vec<Object>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let resp = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_continuation_token(token)
                .send()
                .await
                .map_err(to_fs_err)?;
            out.extend(resp.contents().iter().cloned());
            if resp.is_truncated().unwrap_or(false) {
                token = resp.next_continuation_token().map(String::from);
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// Ranged GET starting at `offset`, streaming to end of object.
    pub async fn get_range(&self, key: &str, offset: u64) -> FsResult<ByteStream> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(format!("bytes={offset}-"))
            .send()
            .await
            .map_err(to_fs_err)?;
        Ok(resp.body)
    }

    /// Upload a whole object in one request (used for small files).
    pub async fn put(&self, key: &str, body: Bytes) -> FsResult<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(to_fs_err)?;
        Ok(())
    }

    pub async fn create_multipart(&self, key: &str) -> FsResult<String> {
        let resp = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(to_fs_err)?;
        resp.upload_id()
            .map(String::from)
            .ok_or(dav_server::fs::FsError::GeneralFailure)
    }

    pub async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
        body: Bytes,
    ) -> FsResult<CompletedPart> {
        let resp = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .body(ByteStream::from(body))
            .send()
            .await
            .map_err(to_fs_err)?;
        Ok(CompletedPart::builder()
            .set_e_tag(resp.e_tag().map(String::from))
            .part_number(part_number)
            .build())
    }

    pub async fn complete_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: Vec<CompletedPart>,
    ) -> FsResult<()> {
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(to_fs_err)?;
        Ok(())
    }

    pub async fn abort_multipart(&self, key: &str, upload_id: &str) -> FsResult<()> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(to_fs_err)?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> FsResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(to_fs_err)?;
        Ok(())
    }

    /// Delete many objects in as few requests as possible. R2's `DeleteObjects`
    /// accepts up to 1000 keys per request, so we chunk accordingly and run the
    /// chunks with bounded concurrency (mirrors `copy_keys` in `fs.rs`).
    pub async fn delete_many(&self, keys: &[String]) -> FsResult<()> {
        // Build one request per chunk first, so a builder failure surfaces as an
        // error rather than being silently dropped.
        let mut requests: Vec<Delete> = Vec::new();
        for chunk in keys.chunks(1000) {
            let objects: Vec<ObjectIdentifier> = chunk
                .iter()
                .filter_map(|k| ObjectIdentifier::builder().key(k).build().ok())
                .collect();
            if objects.is_empty() {
                continue;
            }
            let delete = Delete::builder()
                .set_objects(Some(objects))
                .build()
                .map_err(|_| dav_server::fs::FsError::GeneralFailure)?;
            requests.push(delete);
        }
        stream::iter(requests)
            .map(|delete| async move {
                self.client
                    .delete_objects()
                    .bucket(&self.bucket)
                    .delete(delete)
                    .send()
                    .await
                    .map_err(to_fs_err)?;
                Ok(())
            })
            .buffer_unordered(DELETE_CONCURRENCY)
            .collect::<Vec<FsResult<()>>>()
            .await
            .into_iter()
            .collect()
    }

    /// Server-side copy of a single object.
    pub async fn copy(&self, from_key: &str, to_key: &str) -> FsResult<()> {
        let source = format!(
            "{}/{}",
            self.bucket,
            utf8_percent_encode(from_key, COPY_SET)
        );
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .key(to_key)
            .copy_source(source)
            .send()
            .await
            .map_err(to_fs_err)?;
        Ok(())
    }
}
