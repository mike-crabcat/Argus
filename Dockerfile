# syntax=docker/dockerfile:1.7
# The Argus server plus the browser app, as a small runtime image.
# No configuration or secrets go in the image: mount argus.toml at
# /etc/argus/argus.toml (see deploy/compose.yaml).

ARG RUST_IMAGE=rust:1.99.1-slim-trixie

FROM ${RUST_IMAGE} AS build
# The image's toolchain, not a fresh download for rust-toolchain.toml's
# "stable".
ENV RUSTUP_TOOLCHAIN=1.99.1
RUN rustup target add wasm32-unknown-unknown
# wasm-bindgen's CLI must match the crate version in Cargo.lock exactly.
COPY Cargo.lock /tmp/Cargo.lock
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    v=$(grep -A1 '^name = "wasm-bindgen"$' /tmp/Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p') \
    && cargo install --locked wasm-bindgen-cli --version "$v"
WORKDIR /src
COPY . .
ARG ARGUS_GIT_SHA=unknown
ENV ARGUS_GIT_SHA=${ARGUS_GIT_SHA}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release -p argus-server \
    && scripts/build-web.sh \
    && mkdir -p /out \
    && cp target/release/argus /out/argus \
    && cp -r web/dist /out/web

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home argus
COPY --from=build /out/argus /usr/local/bin/argus
COPY --from=build /out/web /usr/share/argus/web
USER 1000:1000
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD ["argus", "-c", "/etc/argus/argus.toml", "healthcheck"]
ENTRYPOINT ["argus", "-c", "/etc/argus/argus.toml"]
