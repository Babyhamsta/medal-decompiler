//! Coarse timing counters for the recovery scheduler.
//!
//! Counters are incremented once per pass invocation, not per graph node, so
//! the overhead is immaterial next to the work being measured. Reporting is a
//! caller decision; nothing is printed here.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Metric {
    FactsDerive,
    Fingerprint,
    Dominators,
    StructureJumps,
    Inline,
    StructureConditionals,
    RemoveParams,
}

impl Metric {
    pub const COUNT: usize = 7;

    pub const ALL: [Self; Self::COUNT] = [
        Self::FactsDerive,
        Self::Fingerprint,
        Self::Dominators,
        Self::StructureJumps,
        Self::Inline,
        Self::StructureConditionals,
        Self::RemoveParams,
    ];

    const fn index(self) -> usize {
        match self {
            Self::FactsDerive => 0,
            Self::Fingerprint => 1,
            Self::Dominators => 2,
            Self::StructureJumps => 3,
            Self::Inline => 4,
            Self::StructureConditionals => 5,
            Self::RemoveParams => 6,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::FactsDerive => "facts-derive",
            Self::Fingerprint => "fingerprint",
            Self::Dominators => "dominators",
            Self::StructureJumps => "structure-jumps",
            Self::Inline => "inline",
            Self::StructureConditionals => "structure-conditionals",
            Self::RemoveParams => "remove-unnecessary-params",
        }
    }
}

static NANOS: [AtomicU64; Metric::COUNT] = [const { AtomicU64::new(0) }; Metric::COUNT];
static CALLS: [AtomicU64; Metric::COUNT] = [const { AtomicU64::new(0) }; Metric::COUNT];
static ROUNDS: AtomicU64 = AtomicU64::new(0);
static SCHEDULER_RUNS: AtomicU64 = AtomicU64::new(0);

pub fn time<T>(metric: Metric, operation: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = operation();
    let index = metric.index();
    NANOS[index].fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    CALLS[index].fetch_add(1, Ordering::Relaxed);
    value
}

pub fn record_round() {
    ROUNDS.fetch_add(1, Ordering::Relaxed);
}

pub fn record_scheduler_run() {
    SCHEDULER_RUNS.fetch_add(1, Ordering::Relaxed);
}

/// Returns `(label, seconds, calls)` for every metric that was exercised.
pub fn snapshot() -> Vec<(&'static str, f64, u64)> {
    Metric::ALL
        .iter()
        .filter_map(|metric| {
            let index = metric.index();
            let calls = CALLS[index].load(Ordering::Relaxed);
            (calls > 0).then(|| {
                (
                    metric.label(),
                    NANOS[index].load(Ordering::Relaxed) as f64 / 1e9,
                    calls,
                )
            })
        })
        .collect()
}

pub fn rounds() -> u64 {
    ROUNDS.load(Ordering::Relaxed)
}

pub fn scheduler_runs() -> u64 {
    SCHEDULER_RUNS.load(Ordering::Relaxed)
}
