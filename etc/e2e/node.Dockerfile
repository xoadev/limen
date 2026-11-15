# A node for `make e2e`: Debian with sshd and sudo, and procps and iproute2 for the example `system` pack. No systemd:
# its pack is not run here. The base image is pinned by digest, the tag kept to read it, and Dependabot moves it.
FROM debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server sudo procps iproute2 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd \
    && ssh-keygen -A

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
