# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.88

FROM rust:${RUST_VERSION}-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
ARG VERSION=dev
ARG VCS_REF=unknown
LABEL org.opencontainers.image.title="cleanrr" \
      org.opencontainers.image.description="Safely remove stale blocked imports from Radarr and Sonarr queues" \
      org.opencontainers.image.source="https://github.com/Zariel/cleanrr" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}" \
      org.opencontainers.image.revision="${VCS_REF}"

RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates \
    && groupadd --gid 65532 cleanrr \
    && useradd --uid 65532 --gid 65532 --no-create-home --shell /usr/sbin/nologin cleanrr \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /src/target/release/cleanrr /usr/local/bin/cleanrr

USER 65532:65532
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/cleanrr"]
