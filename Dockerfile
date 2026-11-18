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
# OpenH264 (detection's decoder) is C++, built from source by its crate.
RUN apt-get update \
    && apt-get install -y --no-install-recommends g++ \
    && rm -rf /var/lib/apt/lists/*
# wasm-bindgen's CLI must match the crate version in Cargo.lock exactly.
COPY Cargo.lock /tmp/Cargo.lock
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    v=$(grep -A1 '^name = "wasm-bindgen"$' /tmp/Cargo.lock | sed -n 's/^version = "\(.*\)"/\1/p') \
    && cargo install --locked wasm-bindgen-cli --version "$v"
WORKDIR /src
COPY . .
ARG ARGUS_GIT_SHA=unknown
ENV ARGUS_GIT_SHA=${ARGUS_GIT_SHA}
# With CUDA, ONNX Runtime's CUDA provider libraries (downloaded by the ort
# crate at build time) go next to the binary, where it looks for them.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/root/.cache/ort.pyke.io \
    cargo build --release -p argus-server --features cuda \
    && scripts/build-web.sh \
    && mkdir -p /out/bin \
    && cp target/release/argus /out/bin/argus \
    && cp -L target/release/libonnxruntime_providers_shared.so \
             target/release/libonnxruntime_providers_cuda.so /out/bin/ \
    && cp -r web/dist /out/web

# CUDA 13 and cuDNN 9 runtime libraries for detection on an NVIDIA GPU,
# from NVIDIA's redistributable wheels on PyPI, pinned by hash. The host
# needs only the driver (580 or newer) and the NVIDIA container toolkit.
FROM debian:trixie-slim AS cuda
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /w
RUN set -e; mkdir -p /out/lib; \
    while read -r url sha; do \
      curl -fsSL -o w.whl "$url"; \
      echo "$sha  w.whl" | sha256sum -c -; \
      unzip -q -o w.whl 'nvidia/*/lib/*.so*' -d x; \
      rm w.whl; \
    done <<'WHEELS'
https://files.pythonhosted.org/packages/2e/24/d1558f3b68b1d26e706813b1d10aa1d785e4698c425af8db8edc3dced472/nvidia_cuda_runtime-13.0.96-py3-none-manylinux2014_x86_64.manylinux_2_17_x86_64.whl 7f82250d7782aa23b6cfe765ecc7db554bd3c2870c43f3d1821f1d18aebf0548
https://files.pythonhosted.org/packages/28/d4/30e630055d8e0f7058cc0d39e562c965d692f4735537d3f02f45d579a280/nvidia_cublas-13.0.2.14-py3-none-manylinux_2_27_x86_64.whl 0cf238ffdbe46c00cb9aa98e7bca745c223a5ca5a53e686ac8b3e5a08fe80d6c
https://files.pythonhosted.org/packages/ef/6f/94535c450d44a68ef0b43d7a5e54c0e5ecda2ca3db7daec5d54c8d94e0d8/nvidia_cudnn_cu13-9.28.1.4-py3-none-manylinux_2_27_x86_64.whl a54f205b1db1f2895d99f398754db1369816f0569dd459731f925635cf7b65ec
https://files.pythonhosted.org/packages/12/63/636e5dc9afb2959bfc0042a357d5567358df829b66c96d9fc36d77dc0596/nvidia_curand-10.4.5.25-py3-none-manylinux_2_27_x86_64.whl 22bb226a857e5888fe6b54ae465fc4371296bcff13a35f79e19691bcf454f529
WHEELS
RUN find x -name '*.so*' ! -name 'libnvblas*' -exec cp {} /out/lib/ \;

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --create-home argus
COPY --from=cuda /out/lib /usr/local/lib/argus-cuda
COPY --from=build /out/bin/ /usr/local/bin/
COPY --from=build /out/web /usr/share/argus/web
ENV LD_LIBRARY_PATH=/usr/local/lib/argus-cuda
# The container toolkit mounts the driver when the container asks for a GPU.
ENV NVIDIA_VISIBLE_DEVICES=all NVIDIA_DRIVER_CAPABILITIES=compute,utility
USER 1000:1000
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/argus", "-c", "/etc/argus/argus.toml", "healthcheck"]
# By full path: ONNX Runtime finds its CUDA provider next to the binary
# by the path the binary was started with.
ENTRYPOINT ["/usr/local/bin/argus", "-c", "/etc/argus/argus.toml"]
