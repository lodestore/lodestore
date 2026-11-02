FROM debian:bookworm-slim
ARG TARGETARCH
ARG MINIO_RELEASE=RELEASE.2025-09-07T16-13-09Z
ARG MC_RELEASE=RELEASE.2025-08-13T08-35-41Z
ARG MINIO_SHA256_LINUX_AMD64=7c5bd8512c6e966455b1d198209358b2d191c77a83ab377c4073281065fb855f
ARG MINIO_SHA256_LINUX_ARM64=5c83cd2cf151717ba0243f73e1c7802ff36e272b67144bdd7f1f7d684fd6f03d
ARG MC_SHA256_LINUX_AMD64=01f866e9c5f9b87c2b09116fa5d7c06695b106242d829a8bb32990c00312e891
ARG MC_SHA256_LINUX_ARM64=14c8c9616cfce4636add161304353244e8de383b2e2752c0e9dad01d4c27c12c

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl jq \
 && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) minio_sha=$MINIO_SHA256_LINUX_AMD64; mc_sha=$MC_SHA256_LINUX_AMD64 ;; \
      arm64) minio_sha=$MINIO_SHA256_LINUX_ARM64; mc_sha=$MC_SHA256_LINUX_ARM64 ;; \
      *) echo "no pinned MinIO binary for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /usr/local/bin/minio "https://github.com/minio/minio/releases/download/${MINIO_RELEASE}/minio.linux-${TARGETARCH}.${MINIO_RELEASE}"; \
    echo "${minio_sha}  /usr/local/bin/minio" | sha256sum --check --status; \
    curl -fsSL -o /usr/local/bin/mc "https://github.com/minio/mc/releases/download/${MC_RELEASE}/mc.linux-${TARGETARCH}.${MC_RELEASE}"; \
    echo "${mc_sha}  /usr/local/bin/mc" | sha256sum --check --status; \
    chmod 0755 /usr/local/bin/minio /usr/local/bin/mc

COPY --chmod=0755 seed.sh /usr/local/bin/seed.sh
COPY policies/demo.yaml /etc/demo/demo.yaml

RUN mkdir -p /data /seed \
 && chown 10001:10001 /data /seed

USER 10001:10001
ENTRYPOINT ["/usr/local/bin/minio"]
