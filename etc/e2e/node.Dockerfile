# A node for `make e2e`: Debian with sshd and sudo, and procps and iproute2 for the example `system` pack. No systemd:
# its pack is not run here. The base image is pinned by digest, the tag kept to read it, and Dependabot moves it.
FROM debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server sudo procps iproute2 \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /run/sshd \
    && ssh-keygen -A

EXPOSE 22
CMD ["/usr/sbin/sshd", "-D", "-e"]
