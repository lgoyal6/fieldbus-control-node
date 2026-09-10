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
- **Single host.** One Apple M3 Pro under macOS, with other processes running,
  including a multi-hour training job during every run reported below.

## Two experiments, and why there are two

The experiment is frozen in a manifest before any result exists, and the
manifest is what the code reads at run time. There have been two.

**v1** (`manifest/frozen-v1.json`, results under `results/v1/`) failed its own
completion gate. Two of three negative controls fired. The `cpu-hog` control
never did: it ran 22 spinning threads at the *same* Mach time-constraint
policy the control thread had been granted, and 22 peers on 11 cores produced
0 missed deadlines and a p99 of 30 us against a frozen threshold of 1000 us.
The `sensor-freeze` control was caught, but sat on a knife edge of its own
bound: reactions of 3, 9988, 9999 and 10004 us against a 10000 us limit.

Nothing was published, and neither threshold was touched. **v2**
(`manifest/frozen.json`) changes exactly two things and states in the file
itself that the positive gate is byte-identical to v1's, which a test checks
as bytes:

- **A. The hog is now strictly higher priority.** The requirement asks for a
  higher-priority CPU hog. The time-constraint band is the highest a user
  process can request on this OS, so in the `cpu-hog` run *and nowhere else*
  the control thread stays in the band below it. Both sides' requests, grants
  and QoS classes are written into the result file.
- **B. The watchdog budget is eight periods, not ten,** and the reaction is
  measured from the last accepted sensor frame rather than from the expiry of
  the internal budget. That is the literal requirement: the safe state within
  100 ms of the last good reading.

Six short probe runs were made **before** v2 was frozen, and they are recorded
in the manifest's `design_exploration` section, including the one that made
the reporting change necessary: a control thread outside the time-constraint
band costs about 2 ms of wake latency with no hog running at all, which
already exceeds the p99 threshold. So the manifest pre-registers the reading:
on the `cpu-hog` run, a p99 violation is the demotion and the *deadline
misses* are the contention.

## What was measured

From `results/completion.json`, produced by `scripts/run_completion_gate.sh`
at commit `ad31bf0` against the manifest frozen at sha256
`6894f1de0879e48252f4fb8174e090ef13e70b945feea726e6c7ed5e90f20e93`.

Environment: macOS, aarch64, Apple M3 Pro, 11 logical cores, simulated bus and
simulated sensor, real-time clock, Mach `THREAD_TIME_CONSTRAINT_POLICY`
requested and **granted** on both positive runs.

Two positive runs, one before the negative controls and one after, each 10,000
cycles at a 10,000 us period:

| run | p50 | p95 | p99 | max | min | missed deadlines | accepted | rejected |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| positive-1 | 10 us | 20 us | 30 us | 3789 us | 0 us | 0 | 10000 | 0 |
| positive-2 | 10 us | 20 us | 30 us | 142 us | 0 us | 0 | 10000 | 0 |

Percentiles are the upper edge of a 10 us histogram bucket, so a reported `r`
means the true value is in `[r - 10, r)`. `min` and `max` are exact. The
3789 us maximum on the first run is one sample out of ten thousand on a
machine that was also training a model; it is reported because it happened,
and it is not gated because a single wake is not what a deadline gate is for.

### The `cpu-hog` control, which v1 could not catch

22 spinning threads, all 22 granted the Mach time-constraint policy, across
the middle 4,000 cycles, while this run's control thread requests nothing and
stays in the default timeshare band at `QOS_CLASS_USER_INTERACTIVE`. The
spinners report `QOS_CLASS_UNSPECIFIED`, because a thread granted the
time-constraint policy has left the QoS bands entirely. That is the priority
relation, and it is in the result file rather than in this paragraph only.

| | inside the hog window | outside it |
| --- | --- | --- |
| cycles | 3000 through 6999 | 0 through 2999 and 7000 through 9999 |
| missed deadlines | 512 | 6 |
| p50 jitter | 2010 us | 2000 us |
| p99 jitter | 40000 us (lower bound) | 2510 us |
| max jitter | 1177505 us | 16632 us |

Read those two columns together, because separately either one misleads. The
out-of-window column is the same thread, at the same policy, in the same run,
with no hog: it is the baseline. It already fails the 1000 us p99 threshold,
which is the cost of the demotion and not of the contention. What the hog adds
is the rest: 512 of 518 missed deadlines inside the window, and a peak wake
jitter of 1.18 seconds against 16.6 ms outside it.

The in-window p99 of 40000 us is a **lower bound**, not a measurement. The
jitter histogram covers 4000 buckets of 10 us, so samples past 40 ms land in
the overflow bucket; `histogram_overflowed` is `true` in that run's JSON and
the percentile figures are floors. The exact maximum, 1177505 us, is tracked
separately and is exact.

### The watchdog and the validator

The watchdog, under the `sensor-freeze` control: the sensor stops at cycle
5000, and the node entered its safe state **80,053 us after the last accepted
sensor frame**, 19,947 us inside the 100,000 us bound, at cycle 5007. It
latched for the remaining 4,993 cycles and commanded the actuator to 0 for
every cycle after the trip.

The trip landed at cycle 5007 rather than the nominal 5008 because the check
runs at each cycle's actual wake time, not its scheduled one, so the boundary
cycle can move by one either way. That wobble is exactly why the budget is
eight periods: it is now absorbed by 20 ms of margin instead of deciding
whether the run passes.

The validator, under the `can-corrupt` control: 20 injected malformed frames
produced exactly 20 rejections with the exact frozen per-reason histogram
(5 bad CRC, 5 duplicate, 5 out of order, 3 out of range, 2 bad length), while
all 10,000 legitimate frames were still accepted.

## The completion gate passes, and what that is worth

`./scripts/run_completion_gate.sh` exits **0**: both positive runs met every
frozen threshold and all three controls were caught. That is worth exactly as
much as the controls are, so here is what each one actually establishes.

- **`can-corrupt`** establishes the most. An exact per-reason histogram cannot
  be matched by a validator that refuses frames at random.
- **`sensor-freeze`** establishes that the safe state is a state: latched to
  the end of the run, actuator at 0 throughout, inside a bound with margin
  rather than on top of one.
- **`cpu-hog`** establishes that the deadline gate can be made to fail by real
  contention, which v1 could not show. It does **not** establish that this
  machine cannot absorb a same-priority hog. v1 already showed the opposite,
  and that result stands: 22 peers at the same policy did nothing at all.

The gate's own history is part of the evidence and stays in the repository.
`results/v1/` holds the two v1 completion files, including the failing one,
and `manifest/frozen-v1.json` is preserved byte for byte, hash checked by a
test from both sides, so nothing about v1 can be quietly revised now that v2
has a green result.

### What v1 caught that was not a control

Worth reading if you are inclined to trust a green gate. Before v1's honest
failure, `cpu-hog` reported *caught* three times, and all three were false:

- It joined its 22 threads from inside the timed loop at the end of its
  window. The join blocked the loop for 173 ms, which the runner charged to
  wake jitter and then to 170 deadline misses, every one of them after the
  window closed.
- With the join moved out, it spawned its 22 threads from inside the timed
  loop at the start of its window, blocking for 20 ms and producing 4 misses
  in the first two cycles and none across the 4,000 contended cycles after.
- On a third occasion the seven misses were at cycles 9491 through 9494,
  about 2,500 cycles *after* the window closed, with zero inside it: unrelated
  background load on a shared machine.

A fourth defect surfaced alongside them: skipped periods were counted as
`jitter / period` on every late cycle, so each cycle still catching up
re-counted periods already counted, and one 173 ms stall was reported as 153
lost periods instead of 17.

All are fixed, with regression tests. What found them was the
`missed_deadlines.explained` list: a miss carries its cycle index, so a catch
can be audited rather than believed. It is also why v2 reports the in-window
and out-of-window split as a first-class field instead of leaving a reader to
group cycle indices by hand.

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
  runner.rs     periodic scheduler, jitter and deadline accounting,
                scheduling policy and QoS readback
  hog.rs        the cpu-hog negative control
  socketcan.rs  Linux AF_CAN backend (feature `socketcan`, Linux only)
  report.rs     result JSON and gate evaluation
manifest/frozen.json        the experiment in force, v2
manifest/frozen-v1.json     v1, superseded, preserved byte for byte
scripts/run_completion_gate.sh
results/completion.json     the machine-readable evidence for v2
results/v1/                 v1's evidence, including its gate failure
```

## Running it

```sh
# The completion gate. About nine minutes: five 100-second real-time runs
# plus a clean release build. Exits 0 only if both positive runs meet every
# frozen threshold and all three controls are caught.
./scripts/run_completion_gate.sh

# A single positive run.
cargo build --release --workspace
./target/release/fieldbus-node run --cycles 10000 --period-us 10000 \
    --seed 20260909 --out results/my-run.json

# The negative controls. Each is the only way to reach its fault; nothing is
# enabled by default. cpu-hog will make the machine unresponsive for the 40
# seconds of its window, because that is what it is for.
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
