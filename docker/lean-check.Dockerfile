# Container image used by `delphi-p2p check` on platforms without bubblewrap (such as macOS).
# Includes elan and bubblewrap; Lean toolchains are persisted in a Docker volume rather than baked into the image.
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl git bubblewrap \
 && rm -rf /var/lib/apt/lists/*
# Lake mounts an empty filesystem over /run/user inside its sandbox, so the mount point must exist.
# /work/.lake/packages exists and belongs to the checker, so that a volume mounted there does too
# and Lake can still write the rest of .lake.
RUN useradd --create-home --uid 1000 checker && mkdir -p /work/.lake/packages /run/user && chown -R checker /work
USER checker
RUN curl -sSf https://elan.lean-lang.org/elan-init.sh | sh -s -- -y --default-toolchain none
ENV PATH=/home/checker/.elan/bin:$PATH
