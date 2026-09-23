#!/bin/bash
# Build Gyroflow for the browser (Qt 6.7.3 wasm_multithread) into ./dist. Usage: _deployment/web/build.sh
set -eo pipefail
cd "$(dirname "$0")"
source ./env.sh
sed "s|@QTW@|$QTW|g" qtlink.args.in > "$QT_WASM_LINK_ARGS.tmp"
cmp -s "$QT_WASM_LINK_ARGS.tmp" "$QT_WASM_LINK_ARGS" && rm "$QT_WASM_LINK_ARGS.tmp" || mv "$QT_WASM_LINK_ARGS.tmp" "$QT_WASM_LINK_ARGS" # keep mtime: build.rs relinks on change
if ! (cd ../.. && cargo build --release -Zbuild-std=std,panic_abort > "$WEB/build.log" 2>&1); then
  grep -E "^error|@[0-9.]*: .*error:" -A4 build.log | cut -c1-300 | head -60; echo "BUILD FAILED (full log: $WEB/build.log)"; exit 1
fi
OUT=../../target/wasm32-unknown-emscripten/release
mkdir -p dist
cp "$OUT/gyroflow.js" "$OUT/gyroflow.wasm" ../../src/web/index.html ../../src/web/coi-sw.js ../../resources/logo_white.svg ../../resources/icon.svg dist/
# emscripten's (static) pthread worker only lands in a build-script output dir with this cargo layout
cp "$(ls -t $OUT/build/gyroflow-*/out/gyroflow.worker.js $OUT/build/gyroflow/*/out/gyroflow.worker.js 2>/dev/null | head -1)" dist/
cp "$QTW/plugins/platforms/qtloader.js" dist/
ls -la dist/gyroflow.wasm
