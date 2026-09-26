# The hub image (spec §11): the `limen` binary and the SSH client, nothing else. The build context holds the
# binaries already built, `limen-amd64` and `limen-arm64` (what tools/docker.sh lays out); TARGETARCH picks one.
#
# Debian slim and not Alpine: the binary is Kotlin/Native against glibc.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-client \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 7341 --home-dir /data --shell /usr/sbin/nologin limen \
    && mkdir -p /data && chown limen:limen /data

ARG TARGETARCH
COPY --chmod=0755 limen-${TARGETARCH} /usr/local/bin/limen

# /data is LIMEN_HOME (spec §7.2): on first start `limen serve` creates the hub's key, limen.toml and the token of
# MCP clients there, and keeps the invitations of machines joining. One volume is the whole hub. LIMEN_PUBLIC_URL
# —where machines reach the hub to join— belongs to the deployment, not the image.
ENV LIMEN_HOME=/data HOME=/data
VOLUME /data
EXPOSE 7341
USER limen
WORKDIR /data

# Inside a container the server listens on every interface; whoever publishes the port decides who gets in.
ENTRYPOINT ["limen"]
CMD ["serve", "--listen", "0.0.0.0:7341"]
