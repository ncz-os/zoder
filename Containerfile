# syntax=docker/dockerfile:1
# Full zoder stack container: zoder + zerocode + zeroclaw + goose.
#
# Built weekly from master via scripts/package.sh — the SAME version-matched
# bundle the release tarball ships (zoder from this repo; zerocode + zeroclaw
# from the zoder-integration fork; goose CLI, pinned, lean features). Published
# to the GitLab Container Registry and linked from a weekly GitLab Release.
#
# Build args let CI inject a job-token clone URL for the (possibly private)
# zeroclaw fork; goose is public.

FROM rust:1.94-bookworm@sha256:6ae102bdbf528294bc79ad6e1fae682f6f7c2a6e6621506ba959f9685b308a55 AS builder
RUN apt-get update && apt-get install -y --no-install-recommends \
      git curl ca-certificates pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ARG ZEROCLAW_REPO=https://gitlab.com/ncz-os/zeroclaw.git
ARG ZEROCLAW_REF=zoder-integration
ENV ZEROCLAW_REPO=${ZEROCLAW_REPO} \
    ZEROCLAW_REF=${ZEROCLAW_REF} \
    CARGO_TERM_COLOR=never
# Build the native trio + goose for the target platform. buildx sets TARGETARCH
# (amd64|arm64); map it to the Rust triple. With the docker/build-push per-arch
# native-runner pattern (amd64 on ubuntu-latest, arm64 on ubuntu-24.04-arm) each
# platform compiles NATIVELY — no cross-compile, no QEMU-slow arm64 build.
ARG TARGETARCH
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) TRIPLE=x86_64-unknown-linux-gnu ;; \
      arm64) TRIPLE=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    bash scripts/package.sh "$TRIPLE"; \
    mkdir -p /out; \
    tar -xzf dist/zoder-*-"$TRIPLE".tar.gz -C /out --strip-components=1; \
    ls -l /out/zoder /out/zerocode /out/zeroclaw /out/goose

FROM debian:bookworm-slim@sha256:60eac759739651111db372c07be67863818726f754804b8707c90979bda511df
LABEL org.opencontainers.image.source="https://github.com/ncz-os/zoder" \
      org.opencontainers.image.title="zoder-stack" \
      org.opencontainers.image.description="zoder + zerocode + zeroclaw + goose — free-first agentic coding stack" \
      org.opencontainers.image.licenses="Apache-2.0"
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*
# All four version-matched binaries.
COPY --from=builder /out/zoder /out/zerocode /out/zeroclaw /out/goose /usr/local/bin/
# zoder self-heals its corpus/pricing on first run; mount a volume at
# /root/.zoder to persist routing state across container restarts.
ENV ZODER_HOME=/root/.zoder
ENTRYPOINT ["/usr/local/bin/zoder"]
CMD ["--help"]
