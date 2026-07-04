# R2-webdav-rs

A small, self-contained **WebDAV server written in Rust** that stores everything in a
**Cloudflare R2** bucket. It streams data in both directions, so uploading and downloading
**large files** stays memory-bounded regardless of file size.

- WebDAV protocol handled by [`dav-server`](https://crates.io/crates/dav-server)
  (the maintained successor of `webdav-handler-rs`).
- Storage via R2's S3-compatible API, using the AWS SDK (`aws-sdk-s3`).
- **Large downloads**: a single ranged, streamed `GET` per file (supports HTTP `Range`).
- **Large uploads**: S3 **multipart upload** in 8 MiB parts — only one part is held in
  memory at a time. Small files fall back to a single `PutObject`.
- HTTP Basic authentication (single user/password).

## How R2 is mapped to a filesystem

R2 has no directories. A directory is modeled as a zero-byte marker object whose key ends
in `/` (e.g. `photos/`). Listings use `delimiter="/"`, so common prefixes become
sub-directories and objects become files. The bucket root `/` is always a collection.

## Build

```sh
cargo build --release
# binary: target/release/r2-webdav
```

## Configuration (environment variables)

| Variable               | Required | Description                                                        |
| ---------------------- | -------- | ------------------------------------------------------------------ |
| `R2_ACCOUNT_ID`        | *        | Cloudflare account id; used to derive the endpoint URL.            |
| `R2_ENDPOINT`          | *        | Full endpoint, e.g. `https://<acct>.r2.cloudflarestorage.com`. Overrides `R2_ACCOUNT_ID`. |
| `R2_ACCESS_KEY_ID`     | yes      | R2 API token access key id.                                        |
| `R2_SECRET_ACCESS_KEY` | yes      | R2 API token secret access key.                                    |
| `R2_BUCKET`            | yes      | Bucket name.                                                       |
| `WEBDAV_USERNAME`      | yes      | Username clients must present (Basic auth).                        |
| `WEBDAV_PASSWORD`      | yes      | Password clients must present (Basic auth).                        |
| `BIND_ADDR`            | no       | Listen address; default `0.0.0.0:4918`.                            |
| `RUST_LOG`             | no       | Log filter, e.g. `info`, `r2_webdav=debug`.                        |

\* Provide **either** `R2_ENDPOINT` or `R2_ACCOUNT_ID`.

Create the R2 access key in the Cloudflare dashboard: **R2 → Manage API Tokens →
Create API Token** (Object Read & Write).

## Run

```sh
export R2_ACCOUNT_ID=xxxxxxxxxxxxxxxx
export R2_ACCESS_KEY_ID=...
export R2_SECRET_ACCESS_KEY=...
export R2_BUCKET=my-bucket
export WEBDAV_USERNAME=alice
export WEBDAV_PASSWORD=s3cret
export RUST_LOG=info

./target/release/r2-webdav
# WebDAV server (bucket my-bucket) listening on http://0.0.0.0:4918
```

> Basic auth sends credentials in every request. Run behind TLS (a reverse proxy such as
> Caddy/nginx, or a Cloudflare Tunnel) for anything beyond localhost.

## Docker

A small static (musl) Alpine image is published to GitHub Container Registry by the
`Docker Image` workflow — on every GitHub **release**, and on demand via
**workflow_dispatch**.

```sh
docker run --rm -p 4918:4918 \
  -e R2_ACCOUNT_ID=xxxxxxxxxxxxxxxx \
  -e R2_ACCESS_KEY_ID=... \
  -e R2_SECRET_ACCESS_KEY=... \
  -e R2_BUCKET=my-bucket \
  -e WEBDAV_USERNAME=alice \
  -e WEBDAV_PASSWORD=s3cret \
  ghcr.io/conashin/r2-webdav-rs:latest
```

Build it locally:

```sh
docker build -t r2-webdav .
```

## Usage

### curl

```sh
BASE=http://localhost:4918
AUTH='-u alice:s3cret'

# Upload (large files exercise the multipart path)
curl $AUTH -T ./bigfile.bin $BASE/bigfile.bin

# Download and verify round-trip integrity
curl $AUTH -o out.bin $BASE/bigfile.bin && cmp bigfile.bin out.bin

# Partial / ranged download
curl $AUTH -r 0-1023 $BASE/bigfile.bin -o head.bin

# Make a directory, then list it
curl $AUTH -X MKCOL $BASE/docs/
curl $AUTH -X PROPFIND -H 'Depth: 1' $BASE/

# Delete
curl $AUTH -X DELETE $BASE/bigfile.bin
```

### rclone

```sh
rclone config create r2dav webdav \
  url=http://localhost:4918 vendor=other \
  user=alice pass="$(rclone obscure s3cret)"

rclone copy ./big-folder r2dav:/big-folder --progress
```

### Mounting

- **macOS Finder**: Go → Connect to Server → `http://localhost:4918`
- **Windows Explorer**: Map network drive → `http://localhost:4918`

## Notes & limitations

- Directory `COPY`/`MOVE` is implemented by copying each object under the prefix. Very large
  single objects are subject to R2's server-side `CopyObject` size limit.
- Locking uses a "fake" lock system (enough for macOS/Windows clients to work); locks are
  not persisted or enforced across processes.
- An interrupted upload aborts its multipart upload to avoid orphaned parts, but configuring
  an R2 **lifecycle rule to abort incomplete multipart uploads** is still recommended.

## License

MIT — see [LICENSE](LICENSE).
