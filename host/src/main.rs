//! `fieldbus-node`: the command-line entry point.
//!
//! Five subcommands. One runs the loop cleanly; three run it with a single
//! named fault deliberately injected; one assembles the evidence file.
//!
//! The faults are reachable *only* through their own `control` subcommand.
//! There is no configuration file, no environment variable and no default that
//! turns one on, which is the point: a fault that could be left switched on by
//! accident would eventually be left switched on, and every number after that
//! would be wrong in a way nobody noticed.
//!
//! Every run writes a JSON file that states what it measured and what it did
//! not: simulated bus, simulated sensor, single host, no CAN hardware, not
//! hardware in the loop, no RTOS.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use fieldbus_core::node::WATCHDOG_TIMEOUT_US;
use fieldbus_host::bus::{FaultSchedule, SimBus};
use fieldbus_host::hog::logical_cores;
use fieldbus_host::manifest::{Manifest, DEFAULT_PATH};
use fieldbus_host::report::{
    build_run_report, evaluate_can_corrupt, evaluate_cpu_hog, evaluate_sensor_freeze, limitations,
    CompletionReport, ManifestRef, RunReport, Summary,
};
use fieldbus_host::runner::{self, HogWindow, Mode, RunConfig};

const USAGE: &str = "\
fieldbus-node: periodic CAN control node with a simulated bus

USAGE:
  fieldbus-node run [OPTIONS]
  fieldbus-node control cpu-hog [OPTIONS]
  fieldbus-node control can-corrupt [OPTIONS]
  fieldbus-node control sensor-freeze [OPTIONS]
  fieldbus-node report --out PATH --positive PATH... --control PATH...

OPTIONS (run and control):
  --cycles N          periods to execute        (default: manifest loop.cycles)
  --period-us N       period length             (default: manifest loop.target_period_us)
  --seed N            simulated-noise seed      (default: manifest loop.seed)
  --mode MODE         real-time | virtual-time  (default: real-time)
  --manifest PATH     frozen manifest           (default: manifest/frozen.json)
  --run-id NAME       label written into the result file
  --out PATH          where to write the JSON result (required)

The three control subcommands each inject exactly one named fault class and
are the only way to reach any fault. Every mode is a simulation: no CAN
hardware is involved and nothing here is hardware in the loop.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(msg) => {
            eprintln!("fieldbus-node: {msg}");
            ExitCode::from(2)
        }
    }
}

/// Returns `Ok(true)` on a pass, `Ok(false)` on a gate failure, `Err` on
/// misuse. The three exit states are distinct on purpose: a gate that fails
/// is a result, and a command that was typed wrong is not.
fn dispatch(args: &[String]) -> Result<bool, String> {
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..], None),
        Some("control") => {
            let which = args
                .get(1)
                .ok_or_else(|| format!("control needs a name\n\n{USAGE}"))?;
            match which.as_str() {
                "cpu-hog" | "can-corrupt" | "sensor-freeze" => {
                    cmd_run(&args[2..], Some(which.clone()))
                }
                other => Err(format!("unknown control {other:?}\n\n{USAGE}")),
            }
        }
        Some("report") => cmd_report(&args[1..]),
        Some("--help") | Some("-h") | None => {
            print!("{USAGE}");
            Ok(true)
        }
        Some(other) => Err(format!("unknown subcommand {other:?}\n\n{USAGE}")),
    }
}

/// Options shared by `run` and the three controls.
struct Opts {
    cycles: Option<u64>,
    period_us: Option<u64>,
    seed: Option<u64>,
    mode: Mode,
    manifest: PathBuf,
    run_id: Option<String>,
    out: Option<PathBuf>,
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        cycles: None,
        period_us: None,
        seed: None,
        mode: Mode::RealTime,
        manifest: PathBuf::from(DEFAULT_PATH),
        run_id: None,
        out: None,
    };
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = |i: usize| -> Result<&String, String> {
            args.get(i + 1)
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--cycles" => {
                o.cycles = Some(parse_u64(value(i)?, flag)?);
                i += 2;
            }
            "--period-us" => {
                o.period_us = Some(parse_u64(value(i)?, flag)?);
                i += 2;
            }
            "--seed" => {
                o.seed = Some(parse_u64(value(i)?, flag)?);
                i += 2;
            }
            "--mode" => {
                o.mode = match value(i)?.as_str() {
                    "real-time" => Mode::RealTime,
                    "virtual-time" => Mode::Virtual,
                    other => {
                        return Err(format!(
                            "--mode must be real-time or virtual-time, got {other:?}"
                        ))
                    }
                };
                i += 2;
            }
            "--manifest" => {
                o.manifest = PathBuf::from(value(i)?);
                i += 2;
            }
            "--run-id" => {
                o.run_id = Some(value(i)?.clone());
                i += 2;
            }
            "--out" => {
                o.out = Some(PathBuf::from(value(i)?));
                i += 2;
            }
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
    }
    Ok(o)
}

fn parse_u64(s: &str, flag: &str) -> Result<u64, String> {
    s.parse::<u64>()
        .map_err(|e| format!("{flag} wants a non-negative integer, got {s:?}: {e}"))
}

fn cmd_run(args: &[String], control: Option<String>) -> Result<bool, String> {
    let o = parse_opts(args)?;
    let out_path = o.out.clone().ok_or_else(|| {
        "--out is required: a run that writes no result is not evidence".to_string()
    })?;
    let manifest = Manifest::load(&o.manifest)?;
    let manifest_ref = ManifestRef::of(&o.manifest)?;

    let cycles = o.cycles.unwrap_or(manifest.loop_.cycles);
    let period_us = o.period_us.unwrap_or(manifest.loop_.target_period_us);
    let seed = o.seed.unwrap_or(manifest.loop_.seed);

    // The fault schedule comes from the frozen manifest, not from Rust
    // constants. That is what makes the manifest the experiment rather than a
    // description of it.
    let mut faults = FaultSchedule::clean();
    let mut hog_window = None;

    if let Some(id) = control.as_deref() {
        let spec = manifest.control(id)?;
        match id {
            "cpu-hog" => {
                hog_window = Some(HogWindow {
                    // The middle 40 percent, so the run has a clean head and
                    // a clean tail to compare the contended middle against.
                    start_cycle: cycles * 3 / 10,
                    end_cycle: cycles * 7 / 10,
                    threads: 2 * logical_cores(),
                });
            }
            "can-corrupt" => {
                let c = spec
                    .injection_cycles
                    .as_ref()
                    .ok_or("manifest can-corrupt control has no injection_cycles")?;
                faults.bad_crc = c.bad_crc.iter().copied().collect();
                faults.duplicate = c.duplicate.iter().copied().collect();
                faults.out_of_order = c.out_of_order.iter().copied().collect();
                faults.out_of_range = c.out_of_range.iter().copied().collect();
                faults.bad_length = c.bad_length.iter().copied().collect();
            }
            "sensor-freeze" => {
                faults.freeze_at = Some(
                    spec.freeze_at_cycle
                        .ok_or("manifest sensor-freeze control has no freeze_at_cycle")?,
                );
            }
            _ => unreachable!("dispatch already rejected unknown controls"),
        }
    }

    let run_id = o
        .run_id
        .clone()
        .unwrap_or_else(|| control.clone().unwrap_or_else(|| "positive".to_string()));

    eprintln!(
        "fieldbus-node {run_id}: {cycles} cycles at {period_us} us, mode {}, seed {seed}, \
         simulated bus (no CAN hardware, not hardware in the loop)",
        o.mode.as_str()
    );
    if let Some(w) = hog_window {
        eprintln!(
            "  cpu-hog control: {} spinning threads on cycles {}..{}",
            w.threads, w.start_cycle, w.end_cycle
        );
    }
    if !faults.is_clean() {
        eprintln!(
            "  fault injection active: {} extra malformed frames scheduled, freeze_at {:?}",
            faults.injected_count(),
            faults.freeze_at
        );
    }

    let mut bus = SimBus::new(seed, faults);
    let cfg = RunConfig {
        cycles,
        period_us,
        mode: o.mode,
    };
    let outcome = runner::run(&mut bus, cfg, hog_window);

    let mut report = build_run_report(
        &run_id,
        &outcome,
        o.mode,
        period_us,
        &manifest,
        manifest_ref,
        WATCHDOG_TIMEOUT_US,
    );

    // A control's verdict is whether it was caught. A positive run's verdict
    // is whether the gate passed. They are different questions and the exit
    // code answers whichever one was asked.
    let verdict = match control.as_deref() {
        Some("cpu-hog") => {
            let c = evaluate_cpu_hog(&report.gate, outcome.hog);
            let caught = c.caught;
            report.control = Some(c);
            caught
        }
        Some("can-corrupt") => {
            let c = evaluate_can_corrupt(&report.gate, &outcome, manifest.control("can-corrupt")?);
            let caught = c.caught;
            report.control = Some(c);
            caught
        }
        Some("sensor-freeze") => {
            let c =
                evaluate_sensor_freeze(&report.gate, &outcome, manifest.control("sensor-freeze")?);
            let caught = c.caught;
            report.control = Some(c);
            caught
        }
        _ => report.gate.pass,
    };

    write_json(&out_path, &report)?;
    print_summary(&report);
    Ok(verdict)
}

fn print_summary(r: &RunReport) {
    println!("--- {} ---", r.run_id);
    println!(
        "  jitter us: p50={} p95={} p99={} max={} (bucket width {} us, upper-edge bound)",
        r.jitter_us.p50,
        r.jitter_us.p95,
        r.jitter_us.p99,
        r.jitter_us.max,
        r.jitter_us.histogram_bucket_width_us
    );
    println!(
        "  missed deadlines: {} ({} explained{})",
        r.missed_deadlines.count,
        r.missed_deadlines.explained.len(),
        if r.missed_deadlines.explained_truncated {
            ", list truncated"
        } else {
            ""
        }
    );
    for m in r.missed_deadlines.explained.iter().take(10) {
        println!(
            "    miss cycle {} kind {} jitter {} us work {} us overrun {} us",
            m.cycle, m.kind, m.jitter_us, m.work_us, m.overrun_us
        );
    }
    println!(
        "  can: accepted={} rejected={}",
        r.can.accepted, r.can.rejected
    );
    for (name, n) in r.can.rejected_by_reason.iter().filter(|(_, n)| **n > 0) {
        println!("    rejected {name} = {n}");
    }
    println!(
        "  watchdog: tripped={} reason={:?} reaction_time_us={:?} trip_cycle={:?}",
        r.watchdog.tripped, r.watchdog.reason, r.watchdog.reaction_time_us, r.watchdog.trip_cycle
    );
    println!("  scheduling policy: {}", r.environment.scheduling_policy);
    for c in &r.gate.checks {
        println!(
            "  gate {}: expected {} observed {} -> {}",
            c.name,
            c.expected,
            c.observed,
            if c.passed { "pass" } else { "FAIL" }
        );
    }
    println!(
        "  positive gate: {}",
        if r.gate.pass { "pass" } else { "FAIL" }
    );
    if let Some(ctrl) = &r.control {
        for c in &ctrl.caught_checks {
            println!(
                "  control check {}: expected {} observed {} -> {}",
                c.name,
                c.expected,
                c.observed,
                if c.passed { "pass" } else { "FAIL" }
            );
        }
        println!(
            "  control {}: caught = {}",
            ctrl.id,
            if ctrl.caught { "TRUE" } else { "FALSE" }
        );
        println!("  {}", ctrl.note);
    }
}

fn cmd_report(args: &[String]) -> Result<bool, String> {
    let mut out: Option<PathBuf> = None;
    let mut positives: Vec<PathBuf> = Vec::new();
    let mut controls: Vec<PathBuf> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{flag} needs a value"))?;
        match flag {
            "--out" => out = Some(PathBuf::from(value)),
            "--positive" => positives.push(PathBuf::from(value)),
            "--control" => controls.push(PathBuf::from(value)),
            other => return Err(format!("unknown report option {other:?}\n\n{USAGE}")),
        }
        i += 2;
    }
    let out = out.ok_or("report needs --out")?;
    if positives.is_empty() || controls.is_empty() {
        return Err("report needs at least one --positive and one --control".to_string());
    }

    let positive_runs = positives
        .iter()
        .map(|p| read_run(p))
        .collect::<Result<Vec<_>, _>>()?;
    let negative_controls = controls
        .iter()
        .map(|p| read_run(p))
        .collect::<Result<Vec<_>, _>>()?;

    let positive_runs_passed = positive_runs.iter().filter(|r| r.gate.pass).count();
    let controls_caught = negative_controls
        .iter()
        .filter(|r| r.control.as_ref().map(|c| c.caught).unwrap_or(false))
        .count();
    let overall_pass =
        positive_runs_passed == positive_runs.len() && controls_caught == negative_controls.len();

    let first = positive_runs
        .first()
        .ok_or("report needs at least one positive run")?;
    let completion = CompletionReport {
        what: "Machine-readable evidence for the timing, framing and watchdog claims in this \
               repository. Produced by scripts/run_completion_gate.sh from a committed tree. \
               Every number here comes from a simulated in-process CAN bus and a simulated \
               sensor on one Apple Silicon Mac. No CAN hardware, no microcontroller execution, \
               no hardware in the loop, no RTOS."
            .to_string(),
        manifest: first.manifest.clone(),
        environment: first.environment.clone(),
        target_period_us: first.target_period_us,
        cycles: first.cycles,
        positive_runs,
        negative_controls,
        summary: Summary {
            positive_runs_passed,
            positive_runs_total: positives.len(),
            controls_caught,
            controls_total: controls.len(),
            overall_pass,
        },
        limitations: limitations(),
    };

    write_json(&out, &completion)?;
    println!("=== completion gate ===");
    println!(
        "  positive runs passed: {}/{}",
        completion.summary.positive_runs_passed, completion.summary.positive_runs_total
    );
    for r in &completion.positive_runs {
        println!(
            "    {}: gate {} p99={} us misses={} rejected={}",
            r.run_id,
            if r.gate.pass { "pass" } else { "FAIL" },
            r.jitter_us.p99,
            r.missed_deadlines.count,
            r.can.rejected
        );
    }
    println!(
        "  negative controls caught: {}/{}",
        completion.summary.controls_caught, completion.summary.controls_total
    );
    for r in &completion.negative_controls {
        let c = r.control.as_ref();
        println!(
            "    {}: caught = {}",
            r.run_id,
            c.map(|c| c.caught).unwrap_or(false)
        );
    }
    println!("  wrote {}", out.display());
    println!("  overall: {}", if overall_pass { "PASS" } else { "FAIL" });
    Ok(overall_pass)
}

fn read_run(path: &Path) -> Result<RunReport, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read run report {}: {e}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|e| format!("cannot parse run report {}: {e}", path.display()))
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
    }
    let text =
        serde_json::to_string_pretty(value).map_err(|e| format!("cannot serialise result: {e}"))?;
    std::fs::write(path, text + "\n").map_err(|e| format!("cannot write {}: {e}", path.display()))
}
