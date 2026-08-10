# Stage 1: Build.
FROM docker.io/library/rust:1.94-bookworm@sha256:b2fe2c0f26e0e1759752b6b2eb93b119d30a80b91304f5b18069b31ea73eaee8 AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    libudev-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release --locked --bin mujina-minerd

# Stage 2: Runtime
FROM docker.io/library/debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241

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
