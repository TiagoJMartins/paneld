# syntax=docker/dockerfile:1

# paneld as a container.
#
#   docker build -t paneld .
#   docker run -d --name paneld \
#     -p 4444:4444 \
#     -v /srv/paneld/paneld.toml:/etc/paneld/paneld.toml:ro \
#     -v paneld-data:/var/lib/paneld \
#     paneld
#
# Two things about the config matter more in a container than outside one.
#
# `public_base_url` must be reachable *from the panel*, so it is the host's LAN
# or tailnet address — never a container hostname, a service name, or loopback.
# The panel is not on this network and cannot resolve any of those. With
# `-p 4444:4444` above, `http://<host-lan-ip>:4444` is the right value; config
# validation rejects loopback outright but cannot detect a container-internal
# name, so this one is on you.
#
# The content store must land on the volume, or every restart blanks the
# dashboard until each publisher happens to fire again. The default
# `content_path` is relative and the working directory is the volume, so leaving
# it alone already does the right thing; an absolute path must be under
# /var/lib/paneld.
#
# `listen` must be "0.0.0.0:4444" rather than a loopback address, or the port
# publish has nothing to forward to.

# Pinned to the same Debian release as the runtime stage, so the glibc the binary
# links against is the glibc that runs it. A static musl build would make the
# pairing irrelevant, but musl linking and cross-compilation are deliberately out
# of scope for this project.
FROM rust:1.97-slim-bookworm AS build

WORKDIR /src
COPY . .

# The registry and target directories are BuildKit cache mounts rather than image
# layers: they survive between builds, so editing one source file recompiles
# paneld alone instead of its whole dependency graph, and neither ends up in the
# layer history. The binary is copied out because a cache mount is not part of
# the image.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked \
 && cp target/release/paneld /usr/local/bin/paneld

FROM debian:bookworm-slim

# `ca-certificates` is here only so Home Assistant can be reached over HTTPS:
# rustls reads the system trust store, and without this an https `base_url` fails
# with a certificate error while a plain http one works, which is a confusing way
# to find out. Nothing else is needed — the fonts are compiled into the binary,
# so there is no font package and no fontconfig to go wrong.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --user-group --no-create-home paneld \
 && install -d -o paneld -g paneld /var/lib/paneld

COPY --from=build /usr/local/bin/paneld /usr/local/bin/paneld

# Declared so an anonymous volume is created even when nobody remembers to mount
# one, which turns "content did not survive a restart" from silent into obvious.
VOLUME /var/lib/paneld

USER paneld
# Also where a relative `content_path` resolves to, i.e. onto the volume.
WORKDIR /var/lib/paneld
EXPOSE 4444

# The binary is the entrypoint, so subcommands work as arguments:
#   docker run --rm -v ...:/etc/paneld/paneld.toml:ro paneld \
#     --config /etc/paneld/paneld.toml preview kindle /tmp/frame.png
ENTRYPOINT ["paneld"]
CMD ["--config", "/etc/paneld/paneld.toml"]
