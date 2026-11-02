ARG SERVER_IMAGE
FROM ${SERVER_IMAGE} AS server

FROM debian:bookworm-slim AS pick
ARG TARGETARCH
COPY --from=server /usr/share/lodestore/agents/ /agents/
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) arch=x86_64 ;; \
      arm64) arch=aarch64 ;; \
      *) echo "no agent build for $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    install -m 0755 "/agents/lodestore-agent-linux-$arch" /lodestore-agent

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=pick /lodestore-agent /usr/local/bin/lodestore-agent
COPY --chmod=0755 agent/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY --chmod=0755 agent/synth.sh /usr/local/bin/synth.sh
RUN mkdir -p /agent/config /agent/state /agent/data /seed \
 && chown -R 10001:10001 /agent /seed
USER 10001:10001
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
