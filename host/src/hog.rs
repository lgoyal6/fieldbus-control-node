//! The `cpu-hog` negative control: deliberate CPU contention.
//!
//! This exists to answer one question about the timing numbers elsewhere in
//! this repository: is the deadline gate actually measuring anything, or would
//! it pass regardless? A gate that cannot be made to fail is not evidence.
//!
//! Nothing here runs unless the `control cpu-hog` subcommand asks for it. The
//! threads are spawned at a cycle index, stopped at another, and joined before
//! the run ends, so no load outlives the control that requested it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::runner::request_realtime_policy;

/// A pool of spinning threads that can be started and stopped once.
pub struct CpuHog {
    threads: usize,
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    /// How many spinners had the elevated policy granted. Reported rather
    /// than assumed: if the operating system refuses the hog threads the
    /// policy it granted the control thread, then this control is testing
    /// priority rather than contention, and a reader has to be able to tell
    /// those apart when the control does not fire.
    granted: Arc<AtomicUsize>,
}

impl CpuHog {
    /// Prepares a hog of `threads` spinners. Nothing runs until [`CpuHog::start`].
    pub fn new(threads: usize) -> Self {
        CpuHog {
            threads,
            stop: Arc::new(AtomicBool::new(false)),
            handles: Vec::new(),
            granted: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// How many threads this hog will spawn.
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// Spawns the spinners.
    ///
    /// Each one requests the same elevated scheduling policy as the control
    /// thread, with the same period. That is the point of the control: load
    /// from a lower-priority process would only prove that priority works.
    /// Competitors at the *same* priority are what test whether the control
    /// thread's deadline is protected by anything real.
    pub fn start(&mut self, period_us: u64) {
        for _ in 0..self.threads {
            let stop = Arc::clone(&self.stop);
            let granted = Arc::clone(&self.granted);
            self.handles.push(std::thread::spawn(move || {
                if request_realtime_policy(period_us).granted {
                    granted.fetch_add(1, Ordering::Relaxed);
                }
                // A dependent floating-point chain, so the optimiser cannot
                // delete the loop and the core is genuinely busy rather than
                // idling in a branch predictor.
                let mut x = 1.000_001f64;
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..4096 {
                        x = x * 1.000_000_1 + 1e-9;
                        if x > 1e6 {
                            x = 1.000_001;
                        }
                    }
                }
                std::hint::black_box(x);
            }));
        }
    }

    /// How many spinners had the elevated policy granted.
    pub fn granted(&self) -> usize {
        self.granted.load(Ordering::Relaxed)
    }

    /// Whether the spinners are currently running.
    pub fn running(&self) -> bool {
        !self.handles.is_empty()
    }

    /// Tells the spinners to stop, without waiting for them.
    ///
    /// One relaxed atomic store, so this is safe to call from inside the
    /// timed control loop. Joining there is not: the first version of this
    /// control joined 22 threads at the end of its window and the join
    /// blocked the loop for 173 ms, which the runner then charged to wake
    /// jitter. Every deadline miss that run reported came from the teardown
    /// and none from the contention, so the control appeared to be caught
    /// when nothing had actually been detected. Signalling and joining are
    /// separate calls for exactly that reason.
    pub fn signal_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Signals the spinners and joins every one of them.
    ///
    /// Joining rather than detaching is deliberate: the run's own numbers are
    /// meaningless if load from the middle of the run is still on the machine
    /// during the tail. Call this outside the timed path, after the loop.
    pub fn stop(&mut self) {
        if self.handles.is_empty() {
            return;
        }
        self.signal_stop();
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

impl Drop for CpuHog {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Logical core count, used to size the hog at 2x cores.
pub fn logical_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
