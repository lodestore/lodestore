FROM node:24-bookworm-slim AS web
WORKDIR /src/web
RUN corepack enable
COPY web/package.json web/pnpm-lock.yaml ./
RUN pnpm install --frozen-lockfile
COPY web ./
RUN pnpm build

FROM rust:1.99.0-bookworm AS builder
ARG TARGETARCH
ARG ZIGLANG_VERSION=0.17.0
ARG ZIGBUILD_VERSION=0.23.4
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake python3-pip file \
    && rm -rf /var/lib/apt/lists/*
RUN pip install --break-system-packages "ziglang==${ZIGLANG_VERSION}"
RUN cargo install --locked cargo-zigbuild --version "${ZIGBUILD_VERSION}"
RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
WORKDIR /src
COPY . .
COPY --from=web /src/web/dist web/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    case "${TARGETARCH}" in \
        amd64) arch=x86_64 ;; \
        arm64) arch=aarch64 ;; \
        *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && scripts/build-musl.sh /out x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    && scripts/build-musl.sh /out --server "${arch}-unknown-linux-musl" \
    && cp "/out/lodestore-server-linux-${arch}" /out/lodestore-server \
    && mkdir -p /rootfs/var/lib/lodestore

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /out/lodestore-server /usr/local/bin/lodestore-server
COPY --from=builder /out/lodestore-agent-linux-x86_64 /out/lodestore-agent-linux-aarch64 /usr/share/lodestore/agents/
COPY --from=builder --chown=65532:65532 /rootfs/var/lib/lodestore /var/lib/lodestore
ENV LODESTORE_DATA_DIR=/var/lib/lodestore
USER 65532:65532
EXPOSE 8080 8443
HEALTHCHECK CMD ["/usr/local/bin/lodestore-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/lodestore-server", "serve"]
