# Stage 1: Build
FROM docker.io/library/rust:bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    libudev-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release --bin mujina-minerd

# Stage 2: Runtime
FROM docker.io/library/debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libudev1 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home mujina

COPY --from=builder /build/target/release/mujina-minerd /usr/local/bin/

LABEL org.opencontainers.image.source=https://github.com/256foundation/mujina

USER mujina
EXPOSE 7785

CMD ["mujina-minerd"]
