# A node for `make e2e`: Debian with sshd, sudo and git (for `sync`), and nothing limen reads through: no ps, no
# ss. No systemd either: the requests that need it answer with their part in `errors`, which the scenes accept.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server sudo git \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd \
    && ssh-keygen -A

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
