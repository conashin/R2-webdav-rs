mod auth;
mod config;
mod r2;
mod safe_path;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use anyhow::Context;
use bytes::Bytes;
use dav_server::fakels::FakeLs;
use dav_server::DavHandler;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::Semaphore;

use auth::RateLimiter;
use config::Config;
use r2::R2FileSystem;

type BoxedBody = UnsyncBoxBody<Bytes, std::io::Error>;

/// Maximum simultaneous in-flight HTTP connections. Bounds memory and
/// protects R2 from unbounded API fanout under connection flood.
const MAX_CONCURRENT_CONNECTIONS: usize = 1024;
/// Maximum request body size for non-streaming uploads (WebDAV `PUT`/`PROPFIND`
/// bodies, lock XML, etc.). Streaming multipart uploads already chunk at 8 MiB
/// so this only caps the buffering required for single-shot bodies.
const MAX_REQUEST_BODY_BYTES: usize = 100 * 1024 * 1024;
/// File mode applied to the Unix domain socket so a reverse proxy in a matching
/// group can connect. `0660` = rw for owner and group, none for others.
const UDS_MODE: u32 = 0o660;

fn text_body(s: &'static str) -> BoxedBody {
    Full::new(Bytes::from_static(s.as_bytes()))
        .map_err(|never| match never {})
        .boxed_unsync()
}

async fn handle(
    req: Request<Incoming>,
    dav: DavHandler,
    cfg: Arc<Config>,
    rate_limiter: Arc<RateLimiter>,
    peer_ip: String,
) -> Result<Response<BoxedBody>, Infallible> {
    // Guard 1: request body size. Enforced before any auth work so that a
    // malicious peer cannot consume memory by uploading an enormous body
    // attached to an unauthorized request.
    if let Some(cl) = req.headers().get(hyper::header::CONTENT_LENGTH) {
        if let Ok(s) = cl.to_str() {
            if let Ok(n) = s.parse::<usize>() {
                if n > MAX_REQUEST_BODY_BYTES {
                    return Ok(Response::builder()
                        .status(StatusCode::PAYLOAD_TOO_LARGE)
                        .body(text_body("Payload Too Large"))
                        .unwrap());
                }
            }
        }
    } else if req
        .headers()
        .get(hyper::header::TRANSFER_ENCODING)
        .map(|v| v.to_str().unwrap_or("").eq_ignore_ascii_case("chunked"))
        .unwrap_or(false)
    {
        // Chunked uploads must opt into streaming; dav-server already does,
        // so this path is informational. We do not bound the total here.
    }

    // Guard 2: rate limit by client IP.
    let trust_proxy = cfg.trust_proxy;
    let ip = auth::client_ip(req.headers(), &peer_ip, trust_proxy);

    if !rate_limiter.check(&ip) {
        tracing::warn!(ip = %ip, "auth locked out");
        let resp = Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("Retry-After", "300")
            .body(text_body("Too Many Requests"))
            .expect("valid 429 response");
        return Ok(resp);
    }

    if !auth::check(req.headers(), &cfg) {
        rate_limiter.record_failure(&ip);
        let resp = Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("WWW-Authenticate", "Basic realm=\"webdav\"")
            .body(text_body("Unauthorized"))
            .expect("valid 401 response");
        return Ok(resp);
    }

    rate_limiter.record_success(&ip);

    // Guard 3: path normalization at the trust boundary.
    let (mut parts, body) = req.into_parts();
    let req = match safe_path::check(&parts.uri) {
        safe_path::PathCheck::Ok(np) => match safe_path::rewrite(&parts.uri, &np) {
            Some(uri) => {
                parts.uri = uri;
                Request::from_parts(parts, body)
            }
            None => {
                return Ok(Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(text_body("Bad Request"))
                    .unwrap());
            }
        },
        safe_path::PathCheck::Forbidden(reason) => {
            tracing::warn!(reason, ip = %ip, "rejected path traversal");
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(text_body("Forbidden"))
                .unwrap());
        }
    };

    let resp = dav.handle(req).await;
    Ok(resp.map(|body| body.map_err(std::io::Error::other).boxed_unsync()))
}

/// A peer address abstracted for both TCP and Unix-domain connections.
#[derive(Clone)]
enum Peer {
    Tcp(SocketAddr),
    /// Unix-domain socket: there is no IP, so the client-IP limiter falls
    /// back to the literal peer label (typically credential-less; behind a
    /// reverse proxy the `X-Forwarded-For` header supplies the real IP).
    Unix,
}

impl Peer {
    fn as_string(&self) -> String {
        match self {
            Peer::Tcp(addr) => addr.ip().to_string(),
            Peer::Unix => "unix".to_string(),
        }
    }
}

/// All state a per-connection task needs, captured by value and moved into
/// the spawned task. Specialized per-listener variant only for the IO type.
struct ServeCtx {
    dav: DavHandler,
    cfg: Arc<Config>,
    rl: Arc<RateLimiter>,
    peer_ip: String,
    permit: tokio::sync::OwnedSemaphorePermit,
}

/// Drive one HTTP/1 connection to completion. Shared by both listener variants
/// once the IO is wrapped in `TokioIo`. The permit is held for the connection's
/// lifetime, bounding concurrency.
async fn serve_conn<I>(io: I, ctx: ServeCtx)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let ServeCtx {
        dav,
        cfg,
        rl,
        peer_ip,
        permit: _permit,
    } = ctx;
    let service = service_fn(move |req| {
        let dav = dav.clone();
        let cfg = cfg.clone();
        let rl = rl.clone();
        let peer_ip = peer_ip.clone();
        async move { handle(req, dav, cfg, rl, peer_ip).await }
    });
    if let Err(e) = http1::Builder::new()
        // Bound per-connection buffers so a single connection cannot
        // balloon memory with mid-stream junk.
        .max_buf_size(64 * 1024)
        .keep_alive(true)
        .serve_connection(TokioIo::new(io), service)
        .await
    {
        tracing::debug!(error = ?e, "connection error");
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = Arc::new(Config::from_env()?);
    let fs = R2FileSystem::new(&cfg);
    let mut builder = DavHandler::builder()
        .filesystem(Box::new(fs))
        .locksystem(FakeLs::new());
    if let Some(base) = &cfg.public_base_url {
        // Offload file GETs to R2's public endpoint via 302 redirects.
        builder = builder.redirect(true);
        tracing::info!("GET redirect enabled -> {base}");
    }
    let dav = builder.build_handler();
    let rate_limiter = Arc::new(RateLimiter::new());
    // Semaphore bounds concurrent connections. Each connection task holds a
    // permit for its lifetime; when MAX_CONCURRENT_CONNECTIONS are in flight,
    // additional accepts block here (no extra tasks spawned) so memory stays
    // bounded even under a connection flood.
    let conn_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTIONS));

    // Select listener: BIND_SOCKET (Unix domain) takes precedence; falling
    // back to BIND_ADDR (TCP). Setting BIND_SOCKET removes any TCP exposure.
    if let Some(path) = &cfg.bind_socket {
        // Remove a stale socket file so rebinds succeed after a crash.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)
            .with_context(|| format!("failed to bind Unix socket {path}"))?;
        // Apply group-readable mode so a reverse proxy with matching group
        // membership can connect.
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(UDS_MODE);
            let _ = std::fs::set_permissions(path, perms);
        }
        tracing::info!(
            "WebDAV server (bucket {}) listening on unix:{path} (mode {UDS_MODE:#o}, max_conns={MAX_CONCURRENT_CONNECTIONS}, max_body={MAX_REQUEST_BODY_BYTES})",
            cfg.bucket
        );
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = ?e, "uds accept failed");
                    continue;
                }
            };
            let permit = conn_sem.clone().acquire_owned().await?;
            let ctx = ServeCtx {
                dav: dav.clone(),
                cfg: cfg.clone(),
                rl: rate_limiter.clone(),
                peer_ip: Peer::Unix.as_string(),
                permit,
            };
            tokio::spawn(async move {
                serve_conn(stream, ctx).await;
            });
        }
    } else {
        let addr: SocketAddr = cfg
            .bind_addr
            .parse()
            .with_context(|| format!("invalid BIND_ADDR: {}", cfg.bind_addr))?;
        let listener = TcpListener::bind(addr).await?;
        tracing::info!(
            "WebDAV server (bucket {}) listening on http://{addr} (max_conns={MAX_CONCURRENT_CONNECTIONS}, max_body={MAX_REQUEST_BODY_BYTES})",
            cfg.bucket
        );
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(error = ?e, "tcp accept failed");
                    continue;
                }
            };
            let permit = conn_sem.clone().acquire_owned().await?;
            let ctx = ServeCtx {
                dav: dav.clone(),
                cfg: cfg.clone(),
                rl: rate_limiter.clone(),
                peer_ip: Peer::Tcp(peer).as_string(),
                permit,
            };
            tokio::spawn(async move {
                serve_conn(stream, ctx).await;
            });
        }
    }
}
