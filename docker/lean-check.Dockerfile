# Container image used by `delphi-p2p check` on platforms without bubblewrap (such as macOS).
# Includes elan and bubblewrap; Lean toolchains are persisted in a Docker volume rather than baked into the image.
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git bubblewrap \
 && rm -rf /var/lib/apt/lists/*
# Lake mounts an empty filesystem over /run/user inside its sandbox, so the mount point must exist.
RUN useradd --create-home --uid 1000 checker && mkdir -p /work /run/user && chown checker /work
USER checker
RUN curl -sSf https://elan.lean-lang.org/elan-init.sh | sh -s -- -y --default-toolchain none
ENV PATH=/home/checker/.elan/bin:$PATH
