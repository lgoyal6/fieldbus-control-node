# fieldbus-control-node

A periodic CAN control node in Rust: J1939-style framing with an eight-reason
frame validator, a latching sensor-staleness watchdog, wake-jitter and
deadline telemetry, a deterministic in-process bus simulator with a fault
injector, and a Linux SocketCAN backend.

The control half is `#![no_std]`, allocation-free and `unsafe`-free, and
cross-compiles to `thumbv7em-none-eabihf`. The host half owns every clock,
thread, socket and allocation.

## What this is not

Stated first, because everything below is only meaningful with it in view.

- **No CAN hardware.** There is no adapter, no transceiver and no physical bus
  anywhere in this project. Every timing number comes from an in-process
  simulator.
- **Not hardware in the loop.** No device, board, emulator or virtual
  interface stands in for hardware in any measurement here. `results/hil.json`
  does not exist and is not meant to.
- **Not a hard real-time system.** This is a POSIX-hosted periodic loop with
  deadline telemetry. The macOS Mach time-constraint policy is *requested* and
  the grant result is recorded; it is not a guarantee, and the OS may demote
  the thread. There is no admission control and no bounded worst-case
  execution time.
- **No RTOS**, no ISO 26262 process, no ASIL classification, no automotive
  qualification, no certification of any kind.
- **Never run on a microcontroller.** `fieldbus-core` compiles for
  `thumbv7em-none-eabihf`. That is a cross-compile check. Nothing has been
  flashed to or executed on a device, and no number here describes one.
- **Not the J1939 protocol.** This uses the J1939 *identifier layout* and two
  proprietary-B PGNs. There is no transport protocol, no multi-packet
  reassembly and no address claiming.
- **Single host.** One Apple M3 Pro under macOS, with other processes running.

## What was measured

From `results/completion.json`, produced by `scripts/run_completion_gate.sh`
at commit `c966a33` against the manifest frozen at sha256
`e8c0a1d77ad76de81bacc12ffba565207440bcd6d9c5cbdcf84a0e1e24e92220`.

Environment: macOS, aarch64, Apple M3 Pro, 11 logical cores, simulated bus and
simulated sensor, real-time clock, Mach `THREAD_TIME_CONSTRAINT_POLICY`
requested and **granted**.

Two positive runs, one before the negative controls and one after, each 10,000
cycles at a 10,000 us period:

| run | p50 | p95 | p99 | max | min | missed deadlines | accepted | rejected |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| positive-1 | 10 us | 20 us | 30 us | 148 us | 0 us | 0 | 10000 | 0 |
| positive-2 | 10 us | 20 us | 30 us | 149 us | 0 us | 0 | 10000 | 0 |

Percentiles are the upper edge of a 10 us histogram bucket, so a reported `r`
means the true value is in `[r - 10, r)`. `min` and `max` are exact.

The watchdog, measured under the `sensor-freeze` control: tripped **3 us**
past its 100,000 us staleness deadline, latched for the remaining 4,991
cycles, and commanded the actuator to 0 for every cycle after the trip.

The validator, measured under the `can-corrupt` control: 20 injected
malformed frames produced exactly 20 rejections with the exact frozen
per-reason histogram (5 bad CRC, 5 duplicate, 5 out of order, 3 out of range,
2 bad length), while all 10,000 legitimate frames were still accepted.

## The completion gate does not pass

`./scripts/run_completion_gate.sh` exits **1**. Two of three negative controls
were caught. This is a real result and it is not being written around.

The `cpu-hog` control is not caught on this machine. It spawns 22 spinning
threads (2x logical cores), all 22 granted the same elevated scheduling policy
as the control thread, across the middle 4,000 cycles. That produced **zero
missed deadlines and a p99 of 30 us** against a frozen threshold of 1000 us.
The contention is real: the same window burns 7.99 CPU-seconds of user time in
a 1.6 second run that otherwise burns 0.00, and peak jitter reached 1070 us.
It is simply invisible to a p99 over 10,000 samples.

Two honest readings, and this repository does not choose between them for you:

1. An M3 Pro that has granted a time-constraint policy really does absorb
   2x-core-count contention without missing a 10 ms deadline.
2. p99 is the wrong statistic to catch brief contention with. Maximum jitter
   moved by a factor of seven and the gate could not see it.

The threshold has not been changed and the hog has not been made more
aggressive than `manifest/frozen.json` specifies. Changing either after seeing
the result is the thing the frozen manifest exists to prevent. A future
iteration should re-freeze the control's criterion **before** running it
again, not after.

### The control caught the harness three times first

Worth reading if you are inclined to trust a green gate. `cpu-hog` reported
*caught* twice before this, and both were false:

- It joined its 22 threads from inside the timed loop at the end of its
  window. The join blocked the loop for 173 ms, which the runner charged to
  wake jitter and then to 170 deadline misses. Every one of them was after the
  window closed; none were inside it.
- With the join moved out, it spawned its 22 threads from inside the timed
  loop at the start of its window, blocking for 20 ms and producing 4 misses,
  all in the first two cycles of the window and none across the 4,000
  contended cycles that followed.

A third defect surfaced alongside them: skipped periods were counted as
`jitter / period` on every late cycle, so each cycle still catching up
re-counted periods already counted, and one 173 ms stall was reported as 153
lost periods instead of 17.

All three are fixed, with regression tests. What found them was the
`missed_deadlines.explained` list in the result files: a miss carries its
cycle index, so a catch can be audited rather than believed. That is why the
list is there.

## Known limitation: no sequence resynchronisation

A rejected frame never advances the validator's sequence position. That is
what makes fault attribution exact and stops one injected fault cascading into
a second rejection on the next good frame. The cost is that a frame genuinely
lost on the wire would desynchronise the node permanently, and the watchdog
would then trip on starvation rather than the node recovering.

A production node would accept a forward gap and count it. That is not done
here because the rejection rule is frozen in `manifest/frozen.json` and the
`can-corrupt` control asserts an exact per-reason histogram against it;
changing the rule after freezing it would invalidate the control that measures
it. The simulator's `Drop` fault models a sensor that did not transmit, which
advances no counter and leaves no gap, so it does not paper over this.

## Layout

```
core/     fieldbus-core   no_std, no alloc, no unsafe
  j1939.rs      29-bit identifier layout, 8-byte payload codec, CRC-8 J1850,
                validator with eight rejection reasons in a fixed order
  control.rs    first-order low-pass, PI with saturation and anti-windup
  watchdog.rs   latching sensor-staleness watchdog
  telemetry.rs  4000-bucket allocation-free jitter histogram
  node.rs       ControlNode::step, a pure function of time and frames
host/     fieldbus-host   std, binary `fieldbus-node`
  bus.rs        CanBus trait, deterministic SimBus with a fault injector
  runner.rs     periodic scheduler, jitter and deadline accounting
  hog.rs        the cpu-hog negative control
  socketcan.rs  Linux AF_CAN backend (feature `socketcan`, Linux only)
  report.rs     result JSON and gate evaluation
manifest/frozen.json        the experiment, frozen before any result existed
scripts/run_completion_gate.sh
results/completion.json     the machine-readable evidence
```

## Running it

```sh
# The completion gate. About nine minutes: five 100-second real-time runs
# plus a clean release build. Exits 0 only if both positive runs meet every frozen
# threshold and all three controls are caught.
./scripts/run_completion_gate.sh

# A single positive run.
cargo build --release --workspace
./target/release/fieldbus-node run --cycles 10000 --period-us 10000 \
    --seed 20260909 --out results/my-run.json

# The negative controls. Each is the only way to reach its fault; nothing is
# enabled by default.
./target/release/fieldbus-node control cpu-hog       --out /tmp/hog.json
./target/release/fieldbus-node control can-corrupt   --out /tmp/corrupt.json
./target/release/fieldbus-node control sensor-freeze --out /tmp/freeze.json

# Deterministic, no sleeping, jitter zero by construction. Not timing
# evidence, and useful for exactly that reason.
./target/release/fieldbus-node run --mode virtual-time --cycles 1000 \
    --out /tmp/virtual.json

# The cross-compile check.
rustup target add thumbv7em-none-eabihf
cargo build -p fieldbus-core --target thumbv7em-none-eabihf
```

On macOS, Homebrew's `rustup` is keg-only and Homebrew's own `cargo` carries
no cross targets, so put `/opt/homebrew/opt/rustup/bin` first on `PATH`. The
gate script does this itself when that directory exists.

### SocketCAN

The backend is Linux-only, behind an off-by-default `socketcan` feature, with
the dependency pinned to exactly `4.0.0` and declared only for Linux targets,
so a build on macOS never pulls it. It is type-checked locally by
cross-compiling and executed only in CI:

```sh
# Type-check only, from macOS. This has never been run here.
rustup target add aarch64-unknown-linux-gnu
cargo check -p fieldbus-host --features socketcan \
    --target aarch64-unknown-linux-gnu --all-targets

# On Linux, with a virtual interface. vcan0 is a kernel loopback: real
# sockets and a real kernel path, but no bus, and not hardware in the loop.
sudo modprobe vcan
sudo ip link add dev vcan0 type vcan
sudo ip link set up vcan0
cargo test -p fieldbus-host --features socketcan -- --ignored --nocapture
```

## CI

`.github/workflows/ci.yml`, four jobs, each checking one claim and refusing to
imply the others: `cargo fmt` plus `clippy -D warnings` plus the suite; the
Cortex-M cross-compile, labelled as such; the SocketCAN tests against a
`vcan0` the workflow creates; and a virtual-time pipeline check that states in
its own output that it is not timing evidence. Actions are pinned by commit
SHA. CI runs the `can-corrupt` and `sensor-freeze` controls at full length,
because neither depends on a real clock. It does not run `cpu-hog`: contention
means nothing against a clock that is advanced rather than observed.

## License

MIT. Copyright (c) 2026 Laksh Goyal.
