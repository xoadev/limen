# A node for `make e2e`: Debian with sshd and sudo, and the programs the gate reads from. No systemd: the
# requests that need it answer with their part in `errors`, which the scenes accept.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server sudo procps iproute2 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd \
    && ssh-keygen -A

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
