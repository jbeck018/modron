# modron cloud runtime: runs `modron serve` inside a Cloudflare Container.
#
# The image is built from THIS directory (wrangler's build context) and
# copies a prebuilt server binary: build it first, on Ubuntu 24.04 or any
# glibc 2.39 host (the base image is Ubuntu 24.04 with git and CA
# certificates already installed, so the image build needs no network):
#   cargo build --release -p modron-cli --bin modron
#   mkdir -p cloud/container/.build
#   cp target/release/modron cloud/container/.build/modron
# then `npx wrangler deploy` in cloud/container (the workflow does both).
FROM buildpack-deps:noble-scm

COPY .build/modron /usr/local/bin/modron
COPY entrypoint.sh /entrypoint.sh

# Run as an unprivileged user; /workspace is the only tree it writes.
RUN chmod 0755 /usr/local/bin/modron /entrypoint.sh \
    && useradd --create-home --uid 10001 modron \
    && mkdir -p /workspace && chown modron:modron /workspace

ENV MODRON_HOST_KIND=cloud
WORKDIR /workspace
USER modron
ENTRYPOINT ["/entrypoint.sh"]
