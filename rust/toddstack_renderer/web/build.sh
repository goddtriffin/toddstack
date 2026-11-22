#!/usr/bin/env bash
# Builds the wasm, the JS glue and the example bundle into web/pkg and web/dist, and prints sizes.
# Needs: rustup target wasm32-unknown-unknown, wasm-bindgen-cli 0.2.130, wasm-opt, brotli, deno.
set -euo pipefail
cd "$(dirname "$0")/../.."
export CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_OPT_LEVEL=z CARGO_PROFILE_RELEASE_PANIC=abort \
  CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=symbols
stokehold cargo build --release --package toddstack-renderer --target wasm32-unknown-unknown --lib
rm -rf toddstack_renderer/web/pkg toddstack_renderer/web/dist
wasm-bindgen --target web --out-dir toddstack_renderer/web/pkg target/wasm32-unknown-unknown/release/toddstack_renderer.wasm
wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-reference-types \
  --enable-mutable-globals -o toddstack_renderer/web/pkg/toddstack_renderer_bg.wasm toddstack_renderer/web/pkg/toddstack_renderer_bg.wasm
deno bundle --config toddstack_renderer/web/deno.jsonc --output toddstack_renderer/web/dist/example.js toddstack_renderer/web/example.ts
cp toddstack_renderer/assets/Arimo-Bold.ttf toddstack_renderer/web/dist/
cp toddstack_renderer/web/pkg/toddstack_renderer_bg.wasm toddstack_renderer/web/dist/
raw=$(wc -c < toddstack_renderer/web/pkg/toddstack_renderer_bg.wasm)
br=$(brotli -c -q 11 toddstack_renderer/web/pkg/toddstack_renderer_bg.wasm | wc -c)
echo "wasm: ${raw} bytes raw, ${br} bytes brotli"
