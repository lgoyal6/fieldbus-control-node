//! Browser adapter for the actual no_std fieldbus-core crate.
//!
//! The page selects a bounded sensor scenario; this wrapper feeds those frames
//! into the production ControlNode validator, watchdog, and controller.

use std::fmt::Write;
use fieldbus_core::j1939::{sensor_frame, STATUS_OK};
use fieldbus_core::node::{ControlNode, WATCHDOG_TIMEOUT_US};
use wasm_bindgen::prelude::*;

const PERIOD_US: u64 = 10_000;

#[wasm_bindgen]
pub fn run_demo(mode: &str, requested_cycles: u32, requested_fault_cycle: u32) -> String {
    let cycles = requested_cycles.clamp(50, 2_000);
    let fault_cycle = requested_fault_cycle.min(cycles - 1);
    let mut node = ControlNode::with_watchdog_timeout(0, WATCHDOG_TIMEOUT_US);
    let mut samples = String::with_capacity((cycles as usize) * 90);
    let mut last_reaction_us = 0;

    samples.push('[');
    for cycle in 0..cycles {
        let now_us = u64::from(cycle) * PERIOD_US;
        let frozen = mode == "sensor_stale" && cycle >= fault_cycle;
        let frame = sensor_frame(
            cycle as u8,
            ((cycle.wrapping_mul(37) % 1_200) as i32 - 600) as i16,
            STATUS_OK,
        );

        let output = if frozen {
            node.step(now_us, &[])
        } else if mode == "bad_crc" && cycle == fault_cycle {
            let mut corrupt_copy = frame;
            corrupt_copy.data[7] ^= 0x01;
            node.step(now_us, &[frame, corrupt_copy])
        } else {
            node.step(now_us, &[frame])
        };
        let safe = output.safe_state.is_some();
        let reaction_us = output.safe_state.map_or(0, |state| {
            state.time_since_last_accepted_us()
        });
        if safe {
            last_reaction_us = reaction_us;
        }

        if cycle != 0 {
            samples.push(',');
        }
        write!(
            &mut samples,
            "{{\"cycle\":{cycle},\"actuator\":{:.3},\"accepted\":{},\"rejected\":{},\"safe\":{},\"reaction_us\":{reaction_us}}}",
            output.actuator,
            output.accepted,
            output.rejected,
            safe
        )
        .expect("writing to a String cannot fail");
    }
    samples.push(']');

    let last = node.step(u64::from(cycles) * PERIOD_US, &[]);
    let latched = last.safe_state.is_some();
    let safe_cycle = if latched {
        Some(last.safe_state.map(|state| state.reason).map(|reason| match reason {
            fieldbus_core::watchdog::SafeReason::SensorStale { tripped_at_us, .. } => {
                (tripped_at_us / PERIOD_US) as u32
            }
        }).unwrap_or(0))
    } else {
        None
    };
    let summary_reaction_us = last.safe_state.map_or(last_reaction_us, |state| {
        state.time_since_last_accepted_us()
    });

    format!(
        "{{\"mode\":\"{}\",\"period_us\":{PERIOD_US},\"timeout_us\":{WATCHDOG_TIMEOUT_US},\"cycles\":{cycles},\"fault_cycle\":{fault_cycle},\"samples\":{samples},\"summary\":{{\"accepted\":{},\"rejected\":{},\"safe_latched\":{latched},\"safe_cycle\":{},\"reaction_us\":{summary_reaction_us}}}}}",
        if mode == "bad_crc" || mode == "sensor_stale" { mode } else { "normal" },
        last.accepted,
        last.rejected,
        safe_cycle.map_or_else(|| "null".to_string(), |cycle| cycle.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::run_demo;

    #[test]
    fn reports_watchdog_reaction_when_trip_occurs_on_final_check() {
        let result = run_demo("sensor_stale", 50, 42);
        assert!(result.contains("\"safe_latched\":true"));
        assert!(result.contains("\"reaction_us\":90000"));
    }
}
