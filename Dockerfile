# syntax=docker/dockerfile:1.7
#
# Two stages: trunk builds the UI, cargo builds the server with the UI
# compiled in (`embed-assets`), and a debian-slim runtime holds the one
# binary. The same shape as Kayak's image (plan D14).
#
# Started bare, the image serves the retail example (in memory) on :7070:
#
#   docker run -p 7070:7070 tidepool
#   docker run --network host tidepool feed     # sample data, from the image
#
# A real project is mounted and named, with a volume for its data:
#
#   docker run -p 7070:7070 -v ./my-project:/project -v tidepool-data:/data \
#     tidepool serve --config /project --data /data --listen 0.0.0.0:7070

ARG RUST_VERSION=1.95
ARG DEBIAN_VERSION=bookworm

# ---- build ----------------------------------------------------------------
FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} AS builder

RUN rustup target add wasm32-unknown-unknown

# Pinned so a rebuild later is the same build; cargo-binstall fetches the
# prebuilt trunk rather than compiling it.
ARG TRUNK_VERSION=0.21.14
RUN curl -L --proto '=https' --tlsv1.2 -sSf \
  https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash \
  && cargo binstall --no-confirm trunk --version ${TRUNK_VERSION}

WORKDIR /app
COPY . .

ENV RUSTFLAGS="-C strip=symbols"
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
  --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
  --mount=type=cache,target=/app/target,sharing=locked \
  cd frontend && trunk build --release && cd .. \
  && cargo build --release -p tidepool --features embed-assets \
  && mkdir -p /out \
  && cp target/release/tidepool /out/tidepool

# ---- runtime --------------------------------------------------------------
FROM debian:${DEBIAN_VERSION}-slim AS runtime

RUN groupadd --gid 10001 tidepool \
  && useradd --uid 10001 --gid 10001 --home-dir /project --no-create-home tidepool

COPY --from=builder /out/tidepool /usr/local/bin/tidepool
# the AGPL asks for its text to travel with the binary
COPY LICENSE licensing.md /usr/share/tidepool/
COPY frontend/vendor/echarts-LICENSE.txt /usr/share/tidepool/

# The default project: the retail example, writable so changes made in the
# UI can be written back. Mount your own over /project.
COPY --chown=10001:10001 examples/retail /project
RUN install -d -o 10001 -g 10001 /data
WORKDIR /project
USER 10001:10001

EXPOSE 7070
ENTRYPOINT ["/usr/local/bin/tidepool"]
CMD ["serve", "--config", "/project", "--listen", "0.0.0.0:7070"]

LABEL org.opencontainers.image.title="tidepool" \
  org.opencontainers.image.description="A live, associative analytics server" \
  org.opencontainers.image.source="https://github.com/niclasgrahm/tidepool" \
  org.opencontainers.image.licenses="AGPL-3.0-or-later"
