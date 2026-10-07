#!/usr/bin/env bash
set -euo pipefail
fieldbus_root=$(cd "$(dirname "$0")/.." && pwd)
for command in "check rules infra/monitoring/alerts.yml" "test rules infra/monitoring/alerts.test.yml"; do
  # These are fixed repository paths, never caller-supplied shell code.
  read -r -a arguments <<< "$command"
  docker run --rm --memory 256m --cpus 1 --entrypoint /bin/promtool \
    -v "$fieldbus_root:/workspace:ro" -w /workspace prom/prometheus:v3.5.0 "${arguments[@]}"
done
