#!/usr/bin/env bash
set -euo pipefail
tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT
cargo run -q -p fieldbus-cloud -- simulate "$tmp_dir/events.log"
test -s "$tmp_dir/events.log"
cargo test -q -p fieldbus-cloud
echo "cloud smoke: PASS"
