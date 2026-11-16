# modron cloud runtime: runs `modron serve` inside a Cloudflare Container.
# The entrypoint clones the run's repo (the git token arrives per run in the
# environment, from the bridge's wake body) and starts `modron serve` bound to
# loopback. See cloud/container/README.md.
#
# Build from the REPO ROOT (the build needs the workspace manifest):
#   docker build -f cloud/container/Dockerfile -t modron-cloud .
# The workflow .github/workflows/cloud-deploy.yml does this build as a gate.

# Same toolchain as .github/workflows/ci.yml and release.yml.
FROM rust:1.99.1-slim AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y pkg-config musl-tools && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The workspace manifest lists mobile/rust-core as a member; cargo refuses
# to load the workspace without it.
COPY mobile/rust-core ./mobile/rust-core
RUN cargo build --release --locked --target x86_64-unknown-linux-musl -p modron-cli && \
    cp target/x86_64-unknown-linux-musl/release/modron /build/modron

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/modron /usr/local/bin/modron

# Unprivileged user; /workspace is the only tree it writes to.
RUN useradd --create-home --uid 10001 modron && mkdir -p /workspace && chown modron:modron /workspace

COPY cloud/container/entrypoint.sh /entrypoint.sh
RUN chmod 0755 /entrypoint.sh

# Non-secret marker read by modron serve (portal host kind). Secrets are
# never set here: the per-run values arrive as container env at start.
ENV MODRON_HOST_KIND=cloud

WORKDIR /workspace
USER modron
ENTRYPOINT ["/entrypoint.sh"]
