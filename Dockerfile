FROM rust:1.88-slim-bookworm AS builder

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY crates ./crates

RUN cargo build --locked --release -p joyride-oracle

FROM debian:bookworm-slim

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/joyride-oracle /usr/local/bin/joyride-oracle

ENV RUST_LOG=info
# [::] binds dual-stack on Linux: reachable over both IPv4 and IPv6.
# Railway private networking is IPv6-only, so an IPv4-only 0.0.0.0 bind
# is unreachable from other services there.
ENV ORACLE_BIND_ADDR=[::]:8083

EXPOSE 8083

CMD ["joyride-oracle"]
