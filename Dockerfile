# syntax=docker/dockerfile:1

# ---- builder ----------------------------------------------------------------
# rust:alpine targets x86_64-unknown-linux-musl by default, producing a static
# binary. aws-lc-sys ships prebuilt musl bindings, so we only need a C toolchain
# (build-base), cmake and perl to compile the bundled aws-lc C sources.
FROM rust:1-alpine AS builder

RUN apk add --no-cache build-base cmake perl

WORKDIR /app

# Pre-build dependencies as a cacheable layer: compile a stub main so the heavy
# crates (aws-sdk, aws-lc) are cached and only our code recompiles on changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

# Now build the real sources. The release profile (Cargo.toml) applies fat LTO
# and codegen-units=1 for the smallest, most optimized shipped binary.
COPY src ./src
RUN rm -f target/release/r2-webdav target/release/deps/r2_webdav* \
    && touch src/main.rs \
    && cargo build --release --locked \
    && strip target/release/r2-webdav

# ---- runtime ----------------------------------------------------------------
FROM alpine:3

# ca-certificates: rustls loads the system trust store to verify R2's TLS cert.
RUN apk add --no-cache ca-certificates \
    && addgroup -S app \
    && adduser -S -G app app \
    # Socket directory shared between app and the reverse proxy (Caddy).
    # Mount /run/r2-webdav as a tmpfs or a host volume so the proxy can reach it.
    && mkdir -p /run/r2-webdav \
    && chown -R app:app /run/r2-webdav

COPY --from=builder /app/target/release/r2-webdav /usr/local/bin/r2-webdav

USER app

# TCP defaults to 0.0.0.0:4918. To use a Unix domain socket instead, set
# BIND_SOCKET=/run/r2-webdav/r2-webdav.sock and unset/ignore BIND_ADDR. See
# README "Linux domain socket" section for volume mounts.
ENV BIND_ADDR=0.0.0.0:4918
EXPOSE 4918

# Socket directory; mount this into Caddy (or any reverse proxy) to expose the
# socket file that the container writes here when BIND_SOCKET is set.
VOLUME ["/run/r2-webdav"]

ENTRYPOINT ["/usr/local/bin/r2-webdav"]
