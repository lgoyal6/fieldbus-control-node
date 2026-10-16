#!/usr/bin/env bash
# Keeps rustc linked to the LLVM shipped with its active Rust toolchain.
# Cargo calls this wrapper when macOS does not preserve DYLD_LIBRARY_PATH.
set -euo pipefail

compiler="$(rustup which --toolchain "$RUSTUP_TOOLCHAIN" rustc)"
toolchain_lib="$(dirname "$(dirname "$compiler")")/lib"
exec env DYLD_LIBRARY_PATH="$toolchain_lib" "$compiler" "$@"
