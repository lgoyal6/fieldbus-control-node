#!/usr/bin/env bash
#
# The completion gate. This is the exact command that decides whether the
# numbers in results/completion.json may be stated as results.
#
# It runs the positive experiment twice, with the three negative controls in
# between, and passes only if both positive runs satisfy every threshold in
# manifest/frozen.json and all three controls were caught.
#
# manifest/frozen.json is experiment v2. v1 is preserved byte for byte as
# manifest/frozen-v1.json, and the results it produced are under results/v1/,
# including the gate failure that led to v2. v2 changes exactly two things:
# the cpu-hog control now runs its spinners in a strictly higher scheduling
# band than the control thread, and the watchdog budget is eight periods with
# the reaction measured from the last accepted sensor frame. The positive gate
# is byte-identical to v1's and a test asserts that. Running the positive
# case twice, on either side of the controls, is what makes a passing result
# mean something: a single clean run at the start proves nothing about the
# state the machine was left in, and a run that only passes before the controls
# have loaded the machine is a run that got lucky.
#
# Nothing here can weaken the gate. Every threshold is read from the frozen
# manifest by the binary itself, and the manifest's SHA-256 is printed below
# and recorded inside every result file.
#
# What this measures: a simulated in-process CAN bus and a simulated sensor,
# on one host. There is no CAN hardware, no microcontroller execution, no
# hardware in the loop and no RTOS. results/hil.json is deliberately not
# produced, because no hardware exists to produce it.
#
# Usage: ./scripts/run_completion_gate.sh
# Exit:  0 pass, 1 gate failure, 2 misuse.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Homebrew's rustup is keg-only, and Homebrew's own cargo has no cross targets
# installed, so the thumbv7em and Linux checks need the rustup one first on
# PATH. Prepended only when it exists, so this still runs on a CI runner.
if [ -d /opt/homebrew/opt/rustup/bin ]; then
  PATH="/opt/homebrew/opt/rustup/bin:$PATH"
  export PATH
fi

BIN="./target/release/fieldbus-node"
MANIFEST="manifest/frozen.json"
# Printed, not used. A superseded manifest is evidence for the results it
# produced and has no authority over this run.
MANIFEST_V1="manifest/frozen-v1.json"

# Per-run JSON goes to scratch; results/completion.json embeds every run in
# full, so the individual files are working copies rather than evidence.
RUNS=".agent-work/gate"
mkdir -p "$RUNS" results

echo "=============================================================="
echo "fieldbus-control-node completion gate"
echo "started:  $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
echo "repo:     $REPO_ROOT"
echo "commit:   $(git rev-parse HEAD 2>/dev/null || echo 'not a git tree')"
echo "manifest: $MANIFEST (experiment v2)"
echo "sha256:   $(shasum -a 256 "$MANIFEST" | awk '{print $1}')"
echo "supersed: $MANIFEST_V1"
echo "sha256:   $(shasum -a 256 "$MANIFEST_V1" | awk '{print $1}')"
echo "mode:     simulated bus and simulated sensor, single host"
echo "          no CAN hardware, not hardware in the loop, no RTOS"
echo "=============================================================="

echo
echo "[1/10] cargo clean"
cargo clean

echo
echo "[2/10] cargo build --release --workspace"
cargo build --release --workspace

echo
echo "[3/10] cargo test --workspace"
cargo test --workspace

echo
echo "[4/10] cargo build -p fieldbus-core --target thumbv7em-none-eabihf"
echo "       cross-compile check only: this artefact is never flashed or run"
cargo build -p fieldbus-core --target thumbv7em-none-eabihf

echo
echo "[5/10] positive run 1 of 2"
POS1=0
"$BIN" run --manifest "$MANIFEST" --run-id positive-1 --out "$RUNS/positive-1.json" || POS1=$?
echo "       positive-1 exit status: $POS1"

echo
echo "[6/10] negative control 1: cpu-hog"
echo "       the spinners request a strictly higher scheduling band than this"
echo "       run's control thread, which is demoted for this run only. The"
echo "       demotion alone costs wake latency, so read the in-window and"
echo "       out-of-window figures in the result, not just caught or not."
CTRL1=0
"$BIN" control cpu-hog --manifest "$MANIFEST" --run-id cpu-hog --out "$RUNS/cpu-hog.json" || CTRL1=$?
echo "       cpu-hog exit status: $CTRL1 (0 means the control was caught)"

echo
echo "[7/10] negative control 2: can-corrupt"
CTRL2=0
"$BIN" control can-corrupt --manifest "$MANIFEST" --run-id can-corrupt --out "$RUNS/can-corrupt.json" || CTRL2=$?
echo "       can-corrupt exit status: $CTRL2 (0 means the control was caught)"

echo
echo "[8/10] negative control 3: sensor-freeze"
CTRL3=0
"$BIN" control sensor-freeze --manifest "$MANIFEST" --run-id sensor-freeze --out "$RUNS/sensor-freeze.json" || CTRL3=$?
echo "       sensor-freeze exit status: $CTRL3 (0 means the control was caught)"

echo
echo "[9/10] positive run 2 of 2"
POS2=0
"$BIN" run --manifest "$MANIFEST" --run-id positive-2 --out "$RUNS/positive-2.json" || POS2=$?
echo "       positive-2 exit status: $POS2"

echo
echo "[10/10] assembling results/completion.json"
# Assembly happens whether or not the runs passed. A failed gate is still a
# result and still needs a machine-readable record; suppressing the file on
# failure would mean the only durable evidence is of the outcomes that went
# the way somebody hoped.
GATE=0
"$BIN" report \
  --out results/completion.json \
  --positive "$RUNS/positive-1.json" \
  --positive "$RUNS/positive-2.json" \
  --control "$RUNS/cpu-hog.json" \
  --control "$RUNS/can-corrupt.json" \
  --control "$RUNS/sensor-freeze.json" || GATE=$?

echo
echo "=============================================================="
echo "finished: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
if [ "$GATE" -eq 0 ]; then
  echo "COMPLETION GATE: PASS"
else
  echo "COMPLETION GATE: FAIL (exit $GATE)"
  echo "The gate is not to be weakened to make it pass. A control that did"
  echo "not fire, or a positive run that missed a threshold, is a negative"
  echo "result and is reported as one."
fi
echo "=============================================================="
exit "$GATE"
