//! Measure checkpoint capture latency and allocation costs at fleet scale.
//!
//! Run in release mode. This measures owned terminal heaps, not process RSS
//! or pane shells; end-to-end latency still needs a PTY workload.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use uniterm_server::Terminal;

struct CountingAllocator;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn allocated(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Relaxed) + bytes;
    PEAK.fetch_max(live, Relaxed);
    ALLOCATIONS.fetch_add(1, Relaxed);
}

// SAFETY: allocation and deallocation are forwarded with the original layout
// to System. The counters only observe successful allocations.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, layout, size) };
        if !next.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
            allocated(size);
        }
        next
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measure(mut work: impl FnMut(), samples: usize) -> (Duration, usize, usize) {
    let mut durations = Vec::with_capacity(samples);
    let baseline = LIVE.load(Relaxed);
    PEAK.store(baseline, Relaxed);
    ALLOCATIONS.store(0, Relaxed);
    for _ in 0..samples {
        let start = Instant::now();
        work();
        durations.push(start.elapsed());
    }
    let peak = PEAK.load(Relaxed).saturating_sub(baseline);
    let allocations = ALLOCATIONS.load(Relaxed) / samples;
    durations.sort_unstable();
    (
        durations[(samples * 95 / 100).min(samples - 1)],
        peak,
        allocations,
    )
}

fn main() {
    for panes in [20, 45] {
        let baseline = LIVE.load(Relaxed);
        let terminals: Vec<_> = (0..panes)
            .map(|_| {
                let mut terminal = Terminal::new(200, 50);
                for _ in 0..1500 {
                    terminal.feed(b"agent output: short history line\r\n");
                }
                terminal
            })
            .collect();
        let terminal_heap = LIVE.load(Relaxed) - baseline;
        let resolved = measure(
            || {
                let lines: Vec<_> = terminals
                    .iter()
                    .map(|terminal| terminal.grid().export_lines(1000))
                    .collect();
                std::hint::black_box(bincode::serialize(&lines).unwrap());
            },
            12,
        );
        let compact = measure(
            || {
                let captures: Vec<_> = terminals
                    .iter()
                    .map(|terminal| terminal.grid().capture_lines(1000))
                    .collect();
                std::hint::black_box(captures);
            },
            32,
        );
        let worker = measure(
            || {
                let captures: Vec<_> = terminals
                    .iter()
                    .map(|terminal| terminal.grid().capture_lines(1000))
                    .collect();
                std::hint::black_box(bincode::serialize(&captures).unwrap());
            },
            12,
        );
        println!(
            "{panes} panes, 200x50, 1500 short history lines: terminal heap {:.2} MiB",
            terminal_heap as f64 / 1_048_576.0
        );
        for (name, (p95, peak, allocations)) in [
            ("resolved+serialized baseline", resolved),
            ("compact capture", compact),
            ("capture+worker serialization", worker),
        ] {
            println!("  {name}: p95 {:.3} ms; transient peak {:.2} MiB; {allocations} allocations/capture", p95.as_secs_f64() * 1000.0, peak as f64 / 1_048_576.0);
        }
        assert!(
            compact.0 < Duration::from_millis(50),
            "core capture exceeded 50 ms"
        );
        assert!(
            compact.1 < resolved.1 / 2,
            "compact peak must be less than half"
        );
        assert!(
            compact.2 < resolved.2 / 100,
            "capture must avoid per-cell allocations"
        );
        assert!(
            worker.1 < resolved.1 / 2,
            "worker must not expand a fleet of cell strings"
        );
        if panes == 20 {
            assert!(
                terminal_heap < 32 * 1024 * 1024,
                "20-pane terminal heap exceeded 32 MiB"
            );
        }
    }
}
