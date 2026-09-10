//! The `cpu-hog` negative control: deliberate CPU contention.
//!
//! This exists to answer one question about the timing numbers elsewhere in
//! this repository: is the deadline gate actually measuring anything, or would
//! it pass regardless? A gate that cannot be made to fail is not evidence.
//!
//! Nothing here runs unless the `control cpu-hog` subcommand asks for it, and
//! no load outlives the control that requested it.
//!
//! # A strictly higher-priority hog
//!
//! Which scheduling policy the spinners request is a parameter, set by
//! `manifest/frozen.json`. In v1 it was hardcoded to the same Mach
//! time-constraint policy the control thread had been granted, and 22 such
//! peers on 11 cores produced 0 missed deadlines and a p99 of 30 us: an
//! equal-priority competitor cannot preempt the loop, so it never did.
//!
//! The requirement asks for a higher-priority hog. The time-constraint band is
//! the highest a user process can request on macOS, so v2 makes the hog higher
//! by leaving the control thread in the band below it for the duration of that
//! one control run, and records both sides' requests, grants and QoS classes
//! in the result file so the relation can be checked rather than believed.
//! What that costs is stated in the manifest and repeated here: a control
//! thread outside the time-constraint band pays about 2 ms of wake latency
//! from timer coalescing before any hog runs at all, so on the cpu-hog run the
//! p99 threshold is failed by the demotion and the *deadline misses* are what
//! the contention adds. The run's own uncontended head and tail carry that
//! baseline, which is why the outcome reports jitter and misses inside the
//! window and outside it separately.
//!
//! # Everything expensive happens outside the timed loop
//!
//! This is the design constraint that matters, and it was learned the hard
//! way. Twice, this control reported the gate as caught while detecting
//! nothing but itself:
//!
//! - Joining 22 threads at the end of the window, from inside the control
//!   loop, blocked it for 173 ms. The runner charged that to wake jitter and
//!   reported 170 deadline misses, every one of them after the window closed
//!   and none inside it.
//! - Spawning 22 threads at the start of the window, from inside the control
//!   loop, blocked it for 20 ms and produced 4 misses, all at the first cycle
//!   of the window and none during the 4000 contended cycles that followed.
//!
//! So the threads are now created before the loop starts and joined after it
//! ends. The only thing the control loop ever does to this object is flip a
//! state flag and wake the waiters, which is a mutex acquisition and a futex
//! wake. Idle threads block on a condition variable rather than polling, so
//! they contribute nothing at all to the uncontended parts of the run.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::runner::ThreadPolicy;

/// Spawned, waiting, consuming nothing.
const IDLE: u8 = 0;
/// Spinning.
const ACTIVE: u8 = 1;
/// Exit at the next check.
const TERMINATE: u8 = 2;

/// Shared state between the control loop and the spinners.
struct Signal {
    /// Read in the spin loop without locking.
    state: AtomicU8,
    /// Held only around a state change and around an idle wait, so a waiter
    /// cannot miss a wake-up between testing the state and blocking on it.
    lock: Mutex<()>,
    idle: Condvar,
    granted: AtomicUsize,
    /// What a spinner asked for and what band it ended up in, written by the
    /// spinners and read after they are joined. Identical for every spinner,
    /// so last writer wins is not a race that matters.
    policy: Mutex<PolicyObserved>,
}

/// What the spinners' scheduling request actually produced.
#[derive(Debug, Clone, Default)]
struct PolicyObserved {
    requested: String,
    qos_class: String,
}

impl Signal {
    fn set(&self, next: u8) {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.state.store(next, Ordering::Relaxed);
        self.idle.notify_all();
    }
}

/// A pool of spinning threads that can be switched on and off in constant
/// time from inside a timed loop.
pub struct CpuHog {
    threads: usize,
    policy: ThreadPolicy,
    signal: Arc<Signal>,
    handles: Vec<JoinHandle<()>>,
}

impl CpuHog {
    /// Prepares a hog of `threads` spinners, each of which will request
    /// `policy`. Nothing is spawned yet.
    pub fn new(threads: usize, policy: ThreadPolicy) -> Self {
        CpuHog {
            threads,
            policy,
            signal: Arc::new(Signal {
                state: AtomicU8::new(IDLE),
                lock: Mutex::new(()),
                idle: Condvar::new(),
                granted: AtomicUsize::new(0),
                policy: Mutex::new(PolicyObserved::default()),
            }),
            handles: Vec::new(),
        }
    }

    /// How many spinners this hog will run.
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// How many spinners had their requested policy granted.
    ///
    /// Reported rather than assumed: a control whose spinners were refused
    /// the policy they asked for is a different experiment from the one the
    /// manifest froze, and a reader has to be able to tell those apart,
    /// whether or not it fired.
    pub fn policy_granted(&self) -> usize {
        self.signal.granted.load(Ordering::Relaxed)
    }

    /// What a spinner asked the scheduler for, as the request names itself.
    pub fn policy_requested(&self) -> String {
        self.signal
            .policy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .requested
            .clone()
    }

    /// The QoS class a spinner ended up in, read back from the kernel.
    ///
    /// This is the other half of the priority relation the `cpu-hog` control
    /// claims. On macOS a thread granted the time-constraint policy reports
    /// QOS_CLASS_UNSPECIFIED, because it has left the QoS bands the control
    /// thread is still in.
    pub fn qos_class(&self) -> String {
        self.signal
            .policy
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .qos_class
            .clone()
    }

    /// Creates the threads, idle. Call this **before** the timed loop starts.
    ///
    /// Each spinner requests the policy this hog was built with. In v1 that
    /// was the same policy the control thread had been granted, on the
    /// argument that peers at equal priority are the harder test. They were
    /// not a test at all: 22 of them never moved the loop. What the
    /// requirement asks for is a higher-priority hog, so the policy is now a
    /// parameter and the manifest sets both sides.
    pub fn spawn(&mut self, period_us: u64) {
        for _ in 0..self.threads {
            let signal = Arc::clone(&self.signal);
            let policy = self.policy;
            self.handles.push(std::thread::spawn(move || {
                let report = policy.apply(period_us);
                if report.granted {
                    signal.granted.fetch_add(1, Ordering::Relaxed);
                }
                {
                    let mut observed = signal.policy.lock().unwrap_or_else(|e| e.into_inner());
                    observed.requested = report.requested.clone();
                    observed.qos_class = report.qos_class.clone();
                }
                // A dependent floating-point chain, so the optimiser cannot
                // delete the loop and the core is genuinely busy rather than
                // idling in a branch predictor.
                let mut x = 1.000_001f64;
                loop {
                    match signal.state.load(Ordering::Relaxed) {
                        ACTIVE => {
                            let mut i = 0;
                            while i < 4096 {
                                x = x * 1.000_000_1 + 1e-9;
                                if x > 1e6 {
                                    x = 1.000_001;
                                }
                                i += 1;
                            }
                        }
                        TERMINATE => break,
                        _ => {
                            let guard = signal.lock.lock().unwrap_or_else(|e| e.into_inner());
                            // Re-tested under the lock, so a state change that
                            // lands between the load above and this wait
                            // cannot be missed.
                            let _unused = signal
                                .idle
                                .wait_while(guard, |_| signal.state.load(Ordering::Relaxed) == IDLE)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                    }
                }
                std::hint::black_box(x);
            }));
        }
    }

    /// Starts the spinners. Constant time, safe inside the timed loop.
    pub fn activate(&self) {
        self.signal.set(ACTIVE);
    }

    /// Stops the spinners without joining them. Constant time, safe inside
    /// the timed loop.
    pub fn deactivate(&self) {
        self.signal.set(IDLE);
    }

    /// Whether any spinner threads exist.
    pub fn running(&self) -> bool {
        !self.handles.is_empty()
    }

    /// Terminates and joins every spinner. Call this **after** the timed loop.
    ///
    /// Joining rather than detaching is deliberate: the run's own numbers are
    /// meaningless if load from the middle of the run is still on the machine
    /// during the tail.
    pub fn stop(&mut self) {
        if self.handles.is_empty() {
            return;
        }
        self.signal.set(TERMINATE);
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
