#!/usr/bin/env bash
# Builds the Rust controller as browser-loadable WebAssembly.
# Build inputs stay outside demo/, which is the static deployment output.
# The generated wasm-bindgen package is written to demo/pkg for local and Vercel use.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
toolchain="1.98.0"
toolchain_bin="$(dirname "$(rustup which --toolchain "$toolchain" cargo)")"
export PATH="$toolchain_bin:$PATH"
export RUSTUP_TOOLCHAIN="$toolchain"
export RUSTC="$repo_root/scripts/rustc_with_toolchain_lib.sh"
command -v wasm-bindgen >/dev/null
command -v cargo >/dev/null

cd "$repo_root"
CARGO_TARGET_DIR="$repo_root/target/wasm-demo" cargo build --manifest-path wasm/Cargo.toml --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir demo/pkg --out-name fieldbus_demo_wasm target/wasm-demo/wasm32-unknown-unknown/release/fieldbus_demo_wasm.wasm
