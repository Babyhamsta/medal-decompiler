//! Phase-attributed time and allocation accounting for the decompiler.
//!
//! Enabled by the `profiling` feature. Every [`crate::error::catch_phase`]
//! boundary opens a [`scope`], so phase coverage follows the existing error
//! boundaries rather than a parallel set of markers.
//!
//! Time and allocation are attributed *exclusively*: a nested scope suspends
//! its parent, so `structure` does not absorb the cost of the `ssa` work it
//! encloses.

use crate::error::DecompilePhase;

#[cfg(not(feature = "profiling"))]
mod inactive {
    use super::DecompilePhase;

    pub struct Scope;

    pub fn scope(_phase: DecompilePhase, _function: Option<usize>) -> Scope {
        Scope
    }

    pub fn checkpoint(_label: &'static str) {}

    pub fn report_to_stderr() {}
}

#[cfg(not(feature = "profiling"))]
pub(crate) use inactive::{Scope, checkpoint, scope};

#[cfg(not(feature = "profiling"))]
pub use inactive::report_to_stderr;

#[cfg(feature = "profiling")]
mod active {
    use super::DecompilePhase;
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::RefCell,
        collections::HashMap,
        sync::{
            Mutex, OnceLock,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
        time::Instant,
    };

    const NONE_FUNCTION: u64 = u64::MAX;

    static LIVE: AtomicUsize = AtomicUsize::new(0);
    static PEAK_LIVE: AtomicUsize = AtomicUsize::new(0);
    static PEAK_PHASE: AtomicUsize = AtomicUsize::new(DecompilePhase::Unknown.index());
    static PEAK_FUNCTION: AtomicU64 = AtomicU64::new(NONE_FUNCTION);
    static TOTAL_ALLOCATED: AtomicU64 = AtomicU64::new(0);
    static TOTAL_ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

    static CURRENT_PHASE: AtomicUsize = AtomicUsize::new(DecompilePhase::Unknown.index());
    static CURRENT_FUNCTION: AtomicU64 = AtomicU64::new(NONE_FUNCTION);

    static PHASE_NANOS: [AtomicU64; DecompilePhase::COUNT] =
        [const { AtomicU64::new(0) }; DecompilePhase::COUNT];
    static PHASE_BYTES: [AtomicU64; DecompilePhase::COUNT] =
        [const { AtomicU64::new(0) }; DecompilePhase::COUNT];
    static PHASE_ALLOCATIONS: [AtomicU64; DecompilePhase::COUNT] =
        [const { AtomicU64::new(0) }; DecompilePhase::COUNT];
    static PHASE_ENTRIES: [AtomicU64; DecompilePhase::COUNT] =
        [const { AtomicU64::new(0) }; DecompilePhase::COUNT];

    /// Per-function totals, so a single dominant prototype is visible rather
    /// than averaged away.
    #[derive(Clone, Copy, Default)]
    pub struct FunctionTotals {
        pub nanos: u64,
        pub bytes: u64,
        pub allocations: u64,
        pub peak_live_during: usize,
    }

    fn function_totals() -> &'static Mutex<HashMap<u64, FunctionTotals>> {
        static TOTALS: OnceLock<Mutex<HashMap<u64, FunctionTotals>>> = OnceLock::new();
        TOTALS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Live-bytes samples taken at named points in the pipeline, to expose how
    /// much memory is *retained* across phases rather than merely touched.
    fn checkpoints() -> &'static Mutex<Vec<(&'static str, usize, u64)>> {
        static CHECKPOINTS: OnceLock<Mutex<Vec<(&'static str, usize, u64)>>> = OnceLock::new();
        CHECKPOINTS.get_or_init(|| Mutex::new(Vec::new()))
    }

    pub struct TrackingAllocator;

    unsafe impl GlobalAlloc for TrackingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc(layout) };
            if !pointer.is_null() {
                record_allocation(layout.size());
            }
            pointer
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let pointer = unsafe { System.alloc_zeroed(layout) };
            if !pointer.is_null() {
                record_allocation(layout.size());
            }
            pointer
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            unsafe { System.dealloc(pointer, layout) }
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
            if !new_pointer.is_null() {
                if new_size >= layout.size() {
                    record_allocation(new_size - layout.size());
                } else {
                    LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
                }
            }
            new_pointer
        }
    }

    fn record_allocation(size: usize) {
        TOTAL_ALLOCATED.fetch_add(size as u64, Ordering::Relaxed);
        TOTAL_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
        if live > PEAK_LIVE.load(Ordering::Relaxed) {
            PEAK_LIVE.store(live, Ordering::Relaxed);
            PEAK_PHASE.store(CURRENT_PHASE.load(Ordering::Relaxed), Ordering::Relaxed);
            PEAK_FUNCTION.store(CURRENT_FUNCTION.load(Ordering::Relaxed), Ordering::Relaxed);
        }
    }

    struct Frame {
        phase: DecompilePhase,
        function: u64,
        start: Instant,
        start_bytes: u64,
        start_allocations: u64,
    }

    thread_local! {
        static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
    }

    fn open_frame(phase: DecompilePhase, function: u64) -> Frame {
        Frame {
            phase,
            function,
            start: Instant::now(),
            start_bytes: TOTAL_ALLOCATED.load(Ordering::Relaxed),
            start_allocations: TOTAL_ALLOCATIONS.load(Ordering::Relaxed),
        }
    }

    /// Fold a frame's elapsed slice into its phase and function totals.
    fn settle(frame: &Frame) {
        let nanos = frame.start.elapsed().as_nanos() as u64;
        let bytes = TOTAL_ALLOCATED
            .load(Ordering::Relaxed)
            .saturating_sub(frame.start_bytes);
        let allocations = TOTAL_ALLOCATIONS
            .load(Ordering::Relaxed)
            .saturating_sub(frame.start_allocations);

        let index = frame.phase.index();
        PHASE_NANOS[index].fetch_add(nanos, Ordering::Relaxed);
        PHASE_BYTES[index].fetch_add(bytes, Ordering::Relaxed);
        PHASE_ALLOCATIONS[index].fetch_add(allocations, Ordering::Relaxed);

        if frame.function != NONE_FUNCTION {
            let live = LIVE.load(Ordering::Relaxed);
            let mut totals = function_totals().lock().unwrap();
            let entry = totals.entry(frame.function).or_default();
            entry.nanos += nanos;
            entry.bytes += bytes;
            entry.allocations += allocations;
            entry.peak_live_during = entry.peak_live_during.max(live);
        }
    }

    /// Resume the parent frame's measurement window after a child settles.
    fn restart(frame: &mut Frame) {
        frame.start = Instant::now();
        frame.start_bytes = TOTAL_ALLOCATED.load(Ordering::Relaxed);
        frame.start_allocations = TOTAL_ALLOCATIONS.load(Ordering::Relaxed);
    }

    pub struct Scope;

    pub fn scope(phase: DecompilePhase, function: Option<usize>) -> Scope {
        let function = function.map_or(NONE_FUNCTION, |id| id as u64);
        PHASE_ENTRIES[phase.index()].fetch_add(1, Ordering::Relaxed);
        STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some(parent) = stack.last_mut() {
                settle(parent);
                restart(parent);
            }
            stack.push(open_frame(phase, function));
        });
        CURRENT_PHASE.store(phase.index(), Ordering::Relaxed);
        CURRENT_FUNCTION.store(function, Ordering::Relaxed);
        Scope
    }

    impl Drop for Scope {
        fn drop(&mut self) {
            STACK.with(|stack| {
                let mut stack = stack.borrow_mut();
                if let Some(frame) = stack.pop() {
                    settle(&frame);
                }
                match stack.last_mut() {
                    Some(parent) => {
                        restart(parent);
                        CURRENT_PHASE.store(parent.phase.index(), Ordering::Relaxed);
                        CURRENT_FUNCTION.store(parent.function, Ordering::Relaxed);
                    }
                    None => {
                        CURRENT_PHASE
                            .store(DecompilePhase::Unknown.index(), Ordering::Relaxed);
                        CURRENT_FUNCTION.store(NONE_FUNCTION, Ordering::Relaxed);
                    }
                }
            });
        }
    }

    pub fn checkpoint(label: &'static str) {
        let live = LIVE.load(Ordering::Relaxed);
        let allocated = TOTAL_ALLOCATED.load(Ordering::Relaxed);
        checkpoints().lock().unwrap().push((label, live, allocated));
    }

    pub fn report_to_stderr() {
        use std::io::Write;

        let mut out = String::from("{\n  \"phases\": [\n");
        let mut first = true;
        for phase in DecompilePhase::ALL {
            let index = phase.index();
            let entries = PHASE_ENTRIES[index].load(Ordering::Relaxed);
            if entries == 0 {
                continue;
            }
            if !first {
                out.push_str(",\n");
            }
            first = false;
            out.push_str(&format!(
                "    {{\"phase\": \"{}\", \"entries\": {}, \"seconds\": {:.3}, \
                 \"alloc_mb\": {:.1}, \"allocations\": {}}}",
                phase.label(),
                entries,
                PHASE_NANOS[index].load(Ordering::Relaxed) as f64 / 1e9,
                PHASE_BYTES[index].load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
                PHASE_ALLOCATIONS[index].load(Ordering::Relaxed),
            ));
        }
        out.push_str("\n  ],\n");

        let peak_function = PEAK_FUNCTION.load(Ordering::Relaxed);
        let peak_phase = PEAK_PHASE.load(Ordering::Relaxed);
        out.push_str(&format!(
            "  \"peak_live_mb\": {:.1},\n  \"peak_phase\": \"{}\",\n  \"peak_function\": {},\n",
            PEAK_LIVE.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
            DecompilePhase::ALL
                .iter()
                .find(|phase| phase.index() == peak_phase)
                .map_or("unknown", |phase| phase.label()),
            if peak_function == NONE_FUNCTION {
                "null".to_owned()
            } else {
                peak_function.to_string()
            },
        ));
        out.push_str(&format!(
            "  \"total_allocated_mb\": {:.1},\n  \"total_allocations\": {},\n  \"live_at_exit_mb\": {:.1},\n",
            TOTAL_ALLOCATED.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
            TOTAL_ALLOCATIONS.load(Ordering::Relaxed),
            LIVE.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
        ));

        out.push_str("  \"checkpoints\": [\n");
        let recorded = checkpoints().lock().unwrap().clone();
        for (index, (label, live, allocated)) in recorded.iter().enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            out.push_str(&format!(
                "    {{\"label\": \"{}\", \"live_mb\": {:.1}, \"allocated_mb\": {:.1}}}",
                label,
                *live as f64 / (1024.0 * 1024.0),
                *allocated as f64 / (1024.0 * 1024.0),
            ));
        }
        out.push_str("\n  ],\n");

        let mut functions = function_totals()
            .lock()
            .unwrap()
            .iter()
            .map(|(id, totals)| (*id, *totals))
            .collect::<Vec<_>>();
        out.push_str(&format!("  \"function_count\": {},\n", functions.len()));

        functions.sort_unstable_by(|left, right| right.1.bytes.cmp(&left.1.bytes));
        out.push_str("  \"top_functions_by_bytes\": [\n");
        for (index, (id, totals)) in functions.iter().take(15).enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            out.push_str(&format!(
                "    {{\"function\": {}, \"alloc_mb\": {:.1}, \"seconds\": {:.3}, \
                 \"allocations\": {}, \"peak_live_during_mb\": {:.1}}}",
                id,
                totals.bytes as f64 / (1024.0 * 1024.0),
                totals.nanos as f64 / 1e9,
                totals.allocations,
                totals.peak_live_during as f64 / (1024.0 * 1024.0),
            ));
        }
        out.push_str("\n  ],\n");

        functions.sort_unstable_by(|left, right| right.1.nanos.cmp(&left.1.nanos));
        out.push_str("  \"top_functions_by_time\": [\n");
        for (index, (id, totals)) in functions.iter().take(15).enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            out.push_str(&format!(
                "    {{\"function\": {}, \"seconds\": {:.3}, \"alloc_mb\": {:.1}}}",
                id,
                totals.nanos as f64 / 1e9,
                totals.bytes as f64 / (1024.0 * 1024.0),
            ));
        }
        out.push_str("\n  ],\n");

        out.push_str(&format!(
            "  \"scheduler_runs\": {},\n  \"scheduler_rounds\": {},\n",
            cfg::metrics::scheduler_runs(),
            cfg::metrics::rounds(),
        ));
        out.push_str("  \"scheduler_passes\": [\n");
        for (index, (label, seconds, calls)) in cfg::metrics::snapshot().iter().enumerate() {
            if index > 0 {
                out.push_str(",\n");
            }
            out.push_str(&format!(
                "    {{\"pass\": \"{label}\", \"seconds\": {seconds:.3}, \"calls\": {calls}}}"
            ));
        }
        out.push_str("\n  ]\n}\n");

        let _ = std::io::stderr().write_all(out.as_bytes());
    }
}

#[cfg(feature = "profiling")]
pub(crate) use active::{Scope, checkpoint, scope};

#[cfg(feature = "profiling")]
pub use active::{TrackingAllocator, report_to_stderr};
