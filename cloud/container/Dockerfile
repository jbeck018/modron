# modron cloud runtime: runs `modron serve` inside a Cloudflare Container.
# The container clones a repo (via GITHUB_TOKEN), starts modron serve,
# and connects to the relay bridge as the agent side of a session.
#
# Build from the REPO ROOT (the build needs the workspace manifest):
#   docker build -f cloud/container/Dockerfile -t modron-cloud .
# Deploy: wrangler deploy (requires CF Containers beta access)
FROM rust:1.99-slim AS builder
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
RUN apt-get update && apt-get install -y git ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/modron /usr/local/bin/modron

# Run as an unprivileged user; /workspace is the only writable tree it needs.
RUN useradd --create-home --uid 10001 modron && mkdir -p /workspace && chown modron:modron /workspace

# The workspace root: modron clones the repo here and runs serve inside it.
WORKDIR /workspace

# modron serve listens on loopback 7699 (WS) + a Unix socket; nothing is
# exposed outside the container.

# The entrypoint script clones the repo and starts serve. Configured via env vars:
#   GITHUB_REPO    - the repo to clone (required)
#   GITHUB_TOKEN   - a PAT with repo access (required for private repos)
#   BRIDGE_URL     - the relay bridge base (required)
COPY cloud/container/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
USER modron
ENTRYPOINT ["/entrypoint.sh"]
