#!/usr/bin/env sh
# Build an example for the browser and serve it:  ./scripts/web.sh [example] [port]
# Examples: cube (default), voice, webcam. Needs `rustup target add wasm32-unknown-unknown`
# and `cargo install wasm-bindgen-cli`. A microphone or camera needs https or localhost:
# `HTTPS=1 ./scripts/web.sh voice` serves TLS with a self-signed certificate (port 8443).
set -eu
cd "$(dirname "$0")/.."
example="${1:-cube}"
port="${2:-8000}"
cargo build --profile web --example "$example" --target wasm32-unknown-unknown --features wasm
wasm-bindgen --target web --no-typescript --out-dir web --out-name app \
  "target/wasm32-unknown-unknown/web/examples/$example.wasm"
if [ "${HTTPS:-}" = "1" ]; then
  exec python3 scripts/serve.py --https "${2:-8443}"
fi
exec python3 scripts/serve.py "$port"
