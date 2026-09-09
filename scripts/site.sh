#!/usr/bin/env sh
# Assemble the GitHub Pages site into site/: the README as the write-up and the cube, voice
# and webcam examples built for the browser. `./scripts/site.sh [profile]`; the profile is
# `pages` (smallest, slow) by default, `web` for a quick look. Needs the wasm32 target,
# wasm-bindgen-cli, and wasm-opt from binaryen if it is on PATH. Serve the result with
# `python3 scripts/serve.py --https --dir site`.
set -eu
cd "$(dirname "$0")/.."
profile="${1:-pages}"
rm -rf site && mkdir -p site
cp README.md web/site.html site/ && mv site/site.html site/index.html
cp PLAN.md site/ 2>/dev/null || true
for example in cube voice webcam; do
  case "$example" in
    cube) features="wasm" ;;
    *) features="wasm,ui,webrtc" ;;
  esac
  cargo build --profile "$profile" --example "$example" --target wasm32-unknown-unknown --features "$features"
  mkdir -p "site/$example"
  wasm-bindgen --target web --no-typescript --out-dir "site/$example" --out-name app \
    "target/wasm32-unknown-unknown/$profile/examples/$example.wasm"
  if command -v wasm-opt >/dev/null 2>&1; then
    wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-reference-types \
      -o "site/$example/app_bg.wasm" "site/$example/app_bg.wasm"
  fi
  sed "s/EXAMPLE/$example/g" web/demo.html > "site/$example/index.html"
  cp "examples/$example.rs" "site/$example/source.rs"
done
touch site/.nojekyll
du -sh site/*/app_bg.wasm
