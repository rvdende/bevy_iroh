#!/usr/bin/env sh
# Build an example for the browser and serve it:  ./scripts/web.sh [example] [port]
# Examples: cube (default), voice, webcam. Needs `rustup target add wasm32-unknown-unknown`
# and `cargo install wasm-bindgen-cli`. A microphone or camera needs https or localhost.
set -eu
cd "$(dirname "$0")/.."
example="${1:-cube}"
port="${2:-8000}"
case "$example" in
  cube) features="wasm" ;;
  *) features="wasm,ui" ;;
esac
cargo build --profile web --example "$example" --target wasm32-unknown-unknown --features "$features"
wasm-bindgen --target web --no-typescript --out-dir web --out-name app \
  "target/wasm32-unknown-unknown/web/examples/$example.wasm"
echo "serving $example on http://localhost:$port/?join=<ticket>"
python3 -m http.server "$port" --directory web
