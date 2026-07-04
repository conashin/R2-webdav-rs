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

# Now build the real sources.
COPY src ./src
RUN cargo build --release --locked \
    && strip target/release/r2-webdav

# ---- runtime ----------------------------------------------------------------
FROM alpine:3.20

# ca-certificates: rustls loads the system trust store to verify R2's TLS cert.
RUN apk add --no-cache ca-certificates \
    && addgroup -S app \
    && adduser -S -G app app

COPY --from=builder /app/target/release/r2-webdav /usr/local/bin/r2-webdav

USER app
ENV BIND_ADDR=0.0.0.0:4918
EXPOSE 4918

ENTRYPOINT ["/usr/local/bin/r2-webdav"]
