# modron cloud runtime: runs `modron serve` inside a Cloudflare Container.
# The container clones a repo (via GITHUB_TOKEN), starts modron serve,
# and connects to the relay bridge as the agent side of a session.
#
# Build: docker build -t modron-cloud .
# Deploy: wrangler deploy (requires CF Containers beta access)
FROM rust:1.99-slim AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --target x86_64-unknown-linux-musl -p modron-cli && \
    cp target/x86_64-unknown-linux-musl/release/modron /build/modron

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y git ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/modron /usr/local/bin/modron

# The workspace root: modron clones the repo here and runs serve inside it.
WORKDIR /workspace

# modron serve listens on 7699 (WS) + a Unix socket.
EXPOSE 7699

# The entrypoint script clones the repo, starts serve, and registers
# with the relay bridge. Configured via env vars:
#   GITHUB_REPO    - the repo to clone (required)
#   GITHUB_TOKEN   - a PAT with repo access (required for private repos)
#   BRIDGE_URL     - the relay bridge base (required)
#   BRIDGE_KEY     - the share key for the session
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
