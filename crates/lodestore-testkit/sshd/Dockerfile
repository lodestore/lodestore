ARG BASE
FROM ${BASE}

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server socat \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd

COPY sshd_config /etc/ssh/sshd_config
COPY entry.sh /entry.sh
RUN chmod 0755 /entry.sh

EXPOSE 22
ENTRYPOINT ["/entry.sh"]
