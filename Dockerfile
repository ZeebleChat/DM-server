# Builder stage
FROM rust:1.93-slim AS builder

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY Cargo.toml ./
COPY Cargo.lock ./
COPY src ./src

RUN cargo build --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    wget \
    && rm -rf /var/lib/apt/lists/*

RUN mkdir -p /keys

WORKDIR /app

COPY --from=builder /build/target/release/zpulse /usr/local/bin/zpulse

EXPOSE 3002

VOLUME /keys

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD wget -qO- http://localhost:3002/health || exit 1

CMD ["zpulse"]
