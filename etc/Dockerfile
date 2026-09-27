# The hub image (spec §11): the `limen` binary and OpenSSH's client, nothing else. The build context holds the static
# binaries already built, `limen-amd64` and `limen-arm64` (what tools/docker.sh lays out); TARGETARCH picks one.
#
# Alpine: the binary needs no libc of its own, and the image stays small.
FROM alpine:3.22

RUN apk add --no-cache openssh-client-default openssh-keygen \
    && adduser -S -D -H -u 7341 -h /data -s /sbin/nologin limen \
    && mkdir -p /data && chown limen /data && chmod 0700 /data

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
