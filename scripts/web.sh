#!/usr/bin/env sh
# Build the cube example for the browser and serve it: ./scripts/web.sh [port]
# Needs `rustup target add wasm32-unknown-unknown` and `cargo install wasm-bindgen-cli`.
set -eu
cd "$(dirname "$0")/.."
cargo build --example cube --target wasm32-unknown-unknown --features wasm
wasm-bindgen --target web --no-typescript --out-dir web --out-name cube \
  target/wasm32-unknown-unknown/debug/examples/cube.wasm
echo "serving on http://localhost:${1:-8000}/?join=<ticket>"
python3 -m http.server "${1:-8000}" --directory web
