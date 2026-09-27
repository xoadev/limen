# The hub image (spec §11): the `limen` binary and OpenSSH's client, nothing else: no shell, no package manager.
# The build context holds the static binaries already built, `limen-amd64` and `limen-arm64` (what tools/docker.sh
# lays out); TARGETARCH picks one.
#
# OpenSSH's client comes from Alpine, which keeps it patched, with just the libraries it links (musl, libcrypto,
# zlib); the image it runs in is distroless. Both base images are pinned by digest, and Dependabot moves them.
FROM alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8 AS ssh

# /out is the image's own files: ssh and ssh-keygen with what `ldd` says they load, the accounts, and /data. ssh
# wants its user in /etc/passwd, and takes `~` from there: limen's is /data.
RUN apk add --no-cache openssh-client-default openssh-keygen \
    && for binary in /usr/bin/ssh /usr/bin/ssh-keygen; do \
         for file in "$binary" $(ldd "$binary" | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'); do \
           mkdir -p "/out$(dirname "$file")" && cp -L "$file" "/out$file"; \
         done; \
       done \
    && mkdir -p /out/etc /out/data \
    && printf '%s\n' 'root:x:0:0:root:/root:/sbin/nologin' 'nobody:x:65534:65534:nobody:/nonexistent:/sbin/nologin' \
         'limen:x:7341:7341:limen:/data:/sbin/nologin' > /out/etc/passwd \
    && printf '%s\n' 'root:x:0:' 'nobody:x:65534:' 'limen:x:7341:' > /out/etc/group \
    && chown 7341:7341 /out/data && chmod 0700 /out/data

FROM gcr.io/distroless/static-debian12@sha256:d75cdd72874d4790092fcb1b058493ecf6bb5bf2b2b897045b00ff01d91843f2

COPY --from=ssh /out/ /
ARG TARGETARCH
COPY --chmod=0755 limen-${TARGETARCH} /usr/local/bin/limen

# /data is LIMEN_HOME (spec §7.2): on first start `limen serve` creates the hub's key, limen.toml and the token of
# MCP clients there, and keeps the invitations of machines joining. One volume is the whole hub. LIMEN_PUBLIC_URL
# —where machines reach the hub to join— belongs to the deployment, not the image.
ENV LIMEN_HOME=/data HOME=/data
VOLUME /data
EXPOSE 7341
USER 7341:7341
WORKDIR /data

# Inside a container the server listens on every interface; whoever publishes the port decides who gets in.
ENTRYPOINT ["/usr/local/bin/limen"]
CMD ["serve", "--listen", "0.0.0.0:7341"]
