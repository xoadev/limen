# The hub image (spec §11): the `limen` binary and OpenSSH's client, nothing else: no shell, no package manager.
# The build context holds the static binaries already built, `limen-amd64` and `limen-arm64` (what tools/docker.sh
# lays out); TARGETARCH picks one.
#
# OpenSSH's client comes from Alpine, which keeps it patched, with just the libraries it links (musl, libcrypto,
# zlib); the image it runs in is distroless. Both base images are pinned by digest, and Dependabot moves them.
FROM alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8 AS ssh

# /out is the image's own files: ssh and ssh-keygen with what `ldd` says they load, and /data. The user is
# distroless's `nonroot` (65532), already in its /etc/passwd, where ssh looks for it.
RUN apk add --no-cache openssh-client-default openssh-keygen \
    && for binary in /usr/bin/ssh /usr/bin/ssh-keygen; do \
         for file in "$binary" $(ldd "$binary" | awk '{ for (i = 1; i <= NF; i++) if ($i ~ /^\//) print $i }'); do \
           mkdir -p "/out$(dirname "$file")" && cp -L "$file" "/out$file"; \
         done; \
       done \
    && mkdir -p /out/data && chown 65532:65532 /out/data && chmod 0700 /out/data

FROM gcr.io/distroless/static-debian12:nonroot@sha256:afa5c872c891853ca7fcf1f12c3edb23f7eeef36189728842dd51042ff57f7ab

COPY --from=ssh /out/ /
ARG TARGETARCH
COPY --chmod=0755 limen-${TARGETARCH} /usr/local/bin/limen

# /data is LIMEN_HOME (spec §7.2): on first start `limen serve` creates the hub's key, limen.toml and the token of
# MCP clients there, and keeps the invitations of machines joining. One volume is the whole hub. LIMEN_PUBLIC_URL
# —where machines reach the hub to join— belongs to the deployment, not the image.
ENV LIMEN_HOME=/data HOME=/data
VOLUME /data
EXPOSE 7341
USER 65532:65532
WORKDIR /data

# Inside a container the server listens on every interface; whoever publishes the port decides who gets in.
ENTRYPOINT ["/usr/local/bin/limen"]
CMD ["serve", "--listen", "0.0.0.0:7341"]
