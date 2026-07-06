mod auth;
mod config;
mod r2;

use std::convert::Infallible;
use std::net::SocketAddr;
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
use tokio::net::TcpListener;

use config::Config;
use r2::R2FileSystem;

type BoxedBody = UnsyncBoxBody<Bytes, std::io::Error>;

fn text_body(s: &'static str) -> BoxedBody {
    Full::new(Bytes::from_static(s.as_bytes()))
        .map_err(|never| match never {})
        .boxed_unsync()
}

async fn handle(
    req: Request<Incoming>,
    dav: DavHandler,
    cfg: Arc<Config>,
) -> Result<Response<BoxedBody>, Infallible> {
    if !auth::check(req.headers(), &cfg) {
        let resp = Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("WWW-Authenticate", "Basic realm=\"webdav\"")
            .body(text_body("Unauthorized"))
            .expect("valid 401 response");
        return Ok(resp);
    }

    let resp = dav.handle(req).await;
    Ok(resp.map(|body| body.map_err(std::io::Error::other).boxed_unsync()))
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

    let addr: SocketAddr = cfg
        .bind_addr
        .parse()
        .with_context(|| format!("invalid BIND_ADDR: {}", cfg.bind_addr))?;
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(
        "WebDAV server (bucket {}) listening on http://{addr}",
        cfg.bucket
    );

    loop {
        let (stream, peer) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let dav = dav.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| handle(req, dav.clone(), cfg.clone()));
            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                tracing::debug!(?peer, error = ?e, "connection error");
            }
        });
    }
}
