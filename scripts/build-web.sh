#!/bin/sh
# Builds the browser app into web/dist (serve it with [http] web_root).
# Needs: rustup target add wasm32-unknown-unknown, and the wasm-bindgen CLI
# at the same version as the wasm-bindgen crate in Cargo.lock:
#   cargo install wasm-bindgen-cli --version <that version>
# (or point WASM_BINDGEN at a matching binary).
set -eu
cd "$(dirname "$0")/.."
CARGO_PROFILE_RELEASE_OPT_LEVEL=s \
CARGO_PROFILE_RELEASE_LTO=true \
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
  cargo build -p argus-web --release --target wasm32-unknown-unknown
rm -rf web/dist
"${WASM_BINDGEN:-wasm-bindgen}" --target web --no-typescript --out-dir web/dist \
  target/wasm32-unknown-unknown/release/argus_web.wasm
cp crates/argus-web/index.html web/dist/
ls -l web/dist
