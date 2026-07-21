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
| `BIND_ADDR`            | no       | TCP listen address; default `0.0.0.0:4918`. Ignored when `BIND_SOCKET` is set. |
| `BIND_SOCKET`          | no       | Path to a Unix domain socket. When set, the server listens **only** on this UDS (no TCP exposure). The parent directory must be writable by the process; the socket file is created with mode `0660` so a reverse proxy in a matching group can connect. The stale socket file is removed at startup. |
| `TRUST_PROXY`          | no       | Set to `1` when running behind a reverse proxy that overwrites `X-Forwarded-For`. The auth rate limiter then keys on the header value; otherwise it keys on the TCP peer IP (or the literal `unix` label when on a UDS). |
| `R2_PUBLIC_BASE_URL`   | no       | Public base URL for the bucket. When set, file `GET`s are answered with a `302` redirect here (see [GET redirects](#get-redirects)). Must be **HTTPS**, must not be a loopback/private/link-local IP or a cloud metadata endpoint — validated at startup. Empty/unset disables it. |
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

## Linux domain socket (Unix socket)

For deployments where the server sits on the same host as a reverse proxy
(Caddy, nginx), listening on a Unix domain socket avoids a TCP port and keeps
the WebDAV listener off the network namespace entirely.

The Docker image ships a dedicated volume for this purpose: `/run/r2-webdav`.
The socket file is created with mode `0660`, so a reverse proxy running as a
user in the `app` group (or with matching uid/gid) can `connect()` to it.

Run the container with the socket exposed:

```sh
docker run --rm \
  -e R2_ACCOUNT_ID=xxxxxxxxxxxxxxxx \
  -e R2_ACCESS_KEY_ID=... \
  -e R2_SECRET_ACCESS_KEY=... \
  -e R2_BUCKET=my-bucket \
  -e WEBDAV_USERNAME=alice \
  -e WEBDAV_PASSWORD=s3cret \
  -e BIND_SOCKET=/run/r2-webdav/r2-webdav.sock \
  -e TRUST_PROXY=1 \
  -v r2-webdav-run:/run/r2-webdav \
  ghcr.io/conashin/r2-webdav-rs:latest
```

To let Caddy (running on the host, in a container, or as a systemd unit)
connect, mount the same volume into Caddy and point `reverse_proxy` at it:

```caddyfile
# Caddyfile (Caddy v2)
files.example.com {
    reverse_proxy unix//data/r2-webdav/r2-webdav.sock
    # Optional security headers (defense in depth even when the app
    # already sets some; Caddy is the TLS termination point so HSTS
    # belongs here).
    header {
        Strict-Transport-Security "max-age=31536000; includeSubDomains"
        X-Content-Type-Options "nosniff"
        Referrer-Policy "strict-origin-when-cross-origin"
    }
}
```

When running both containers in Docker Compose, share the same named volume
and set the Caddy service's group/`user` so it can read the socket, or mount
the volume with `mode=0660`:

```yaml
services:
  r2-webdav:
    image: ghcr.io/conashin/r2-webdav-rs:latest
    environment:
      R2_ACCOUNT_ID: ...
      R2_ACCESS_KEY_ID: ...
      R2_SECRET_ACCESS_KEY: ...
      R2_BUCKET: my-bucket
      WEBDAV_USERNAME: alice
      WEBDAV_PASSWORD: s3cret
      BIND_SOCKET: /run/r2-webdav/r2-webdav.sock
      TRUST_PROXY: "1"
    volumes:
      - r2-webdav-run:/run/r2-webdav

  caddy:
    image: caddy:2
    volumes:
      - r2-webdav-run:/data/r2-webdav:ro
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
    ports:
      - "443:443"
      - "80:80"

volumes:
  r2-webdav-run:
```

> [!NOTE]
> `TRUST_PROXY=1` is required when running behind a reverse proxy so the
> per-IP auth rate limiter reads the client IP from `X-Forwarded-For` instead
> of seeing every request as coming from the proxy (or the literal `unix`
> peer label). Make sure the proxy **overwrites** `X-Forwarded-For` so a
> client cannot forge it.

## Usage

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

## GET redirects

By default every download is streamed through this server. If your bucket is
reachable at a public URL — an [R2 public bucket](https://developers.cloudflare.com/r2/buckets/public-buckets/)
`https://pub-<hash>.r2.dev`, or a custom domain — set `R2_PUBLIC_BASE_URL` and
file `GET`s are answered with a `302` redirect to `<base>/<key>` instead. The
client fetches the bytes directly from R2 (and, with a custom domain, through
Cloudflare's CDN), so downloads no longer consume this server's bandwidth or CPU.

```sh
export R2_PUBLIC_BASE_URL=https://files.example.com
# or: export R2_PUBLIC_BASE_URL=https://pub-xxxxxxxx.r2.dev
```

Only `GET` is redirected; `PROPFIND`, `PUT`, `MKCOL`, `DELETE`, etc. are still
served normally. Leaving the variable empty or unset keeps everything streaming
through the server.

> [!WARNING]
> The redirect target is a public URL, so it is **not** protected by this
> server's Basic auth — anyone with the redirected URL can fetch the object.
> Only enable this if the bucket's contents may be served publicly.

## Security hardening

This server is designed to sit behind a TLS-terminating reverse proxy (Caddy,
nginx, Cloudflare Tunnel) and applies defense-in-depth both at the network
boundary and inside the process:

- **Path normalization at the trust boundary.** All WebDAV request paths are
  percent-decoded and validated before being handed to `dav-server`. Empty
  segments and `.` are dropped; `..` is rejected outright (`403 Forbidden`),
  and double-encoded traversal (`%2f`, `%2e%2e`) is caught after decode.
  See `src/safe_path.rs`.
- **Per-IP authentication rate limiting.** Failed Basic-auth attempts are
  tracked per client IP (`X-Forwarded-For` when `TRUST_PROXY=1`, else the TCP
  peer IP). After 10 failures in a 60-second window the IP is locked out for
  300 seconds. Successful auth resets the counter.
- **Request body size cap.** `Content-Length` is checked against a 100 MiB
  ceiling before any auth or path work, so an unauthorized peer cannot force
  the server to buffer arbitrary data.
- **Connection concurrency limit.** A `Semaphore` caps simultaneous in-flight
  connections at 1024; additional `accept()`s block until a permit is freed.
  Per-connection hyper buffer is also bounded.
- **SSRF validation.** `R2_PUBLIC_BASE_URL` (used for `GET` redirects) is
  validated at startup: HTTPS-only, no userinfo, and must not resolve to a
  loopback/private/link-local address or any well-known cloud metadata
  endpoint (`169.254.169.254`, `metadata.google.internal`, `metadata.azure.com`).
- **Object integrity.** `PUT` and multipart `UploadPart` send a SHA-256
  checksum so R2 can detect in-transit corruption; the SDK's
  `ResponseChecksumValidation::WhenRequired` is enabled for downloads.
- **Constant-time credential comparison.** `subtle::ConstantTimeEq` compares
  SHA-256 digests of both username and password, preventing timing leaks of
  the credential length or prefix.
- **Dependency auditing.** CI runs `cargo audit` and `cargo deny` on every
  push and pull request.

When `BIND_SOCKET` is set (Unix domain socket mode), the server has no TCP
exposure at all — only the reverse proxy connecting to the socket can reach
it. Pair with `TRUST_PROXY=1` and a proxy that overwrites `X-Forwarded-For`.

## Notes & limitations

- Directory `COPY`/`MOVE` is implemented by copying each object under the prefix. Very large
  single objects are subject to R2's server-side `CopyObject` size limit.
- Locking uses a "fake" lock system (enough for macOS/Windows clients to work); locks are
  not persisted or enforced across processes.
- An interrupted upload aborts its multipart upload to avoid orphaned parts, but configuring
  an R2 **lifecycle rule to abort incomplete multipart uploads** is still recommended.

## License

MIT — see [LICENSE](LICENSE).
