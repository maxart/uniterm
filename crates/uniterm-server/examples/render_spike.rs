//! Phase 0 renderer spike: measure the damage-tracked renderer against the
//! budget, at 1 pane and at 45 panes (the diagnostic's real workload).
//!
//! Run: `cargo run --release -p uniterm-server --example render_spike`
//!
//! It checks five properties from `docs/00-vision-and-scope.md`:
//!   1. Zero output when nothing changed (the idle-frame budget).
//!   2. Damage tracking emits a tiny diff, not a full-screen redraw.
//!   3. A render step is far under the 50 ms input-to-pixel budget - this
//!      measures only the grid->escape-sequence step; PTY read + parse are
//!      added in Phase 1, but this proves the render stage has enormous margin.
//!   4. Whole-width scrolling emits a terminal scroll plus one exposed row.
//!   5. Sparse history and persistence omit unused trailing cells.

use std::time::Instant;

use uniterm_core::{Cell, Color, Grid};
use uniterm_server::Renderer;

const COLS: u16 = 200;
const ROWS: u16 = 50;
const FRAMES: usize = 100_000;
const LATENCY_BUDGET_NS: u128 = 50_000_000; // 50 ms, the p95 input-to-pixel budget

fn cell(ch: char, fg: u8) -> Cell {
    Cell {
        ch,
        fg: Color::Idx(fg),
        ..Cell::default()
    }
}

/// A tiny deterministic PRNG so the spike needs no dependency and is repeatable.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u16) -> u16 {
        (self.next() % n as u64) as u16
    }
}

fn main() {
    println!("== Uniterm renderer spike ==");
    println!("grid: {COLS}x{ROWS}, frames: {FRAMES}\n");

    check_zero_output_when_idle();
    check_damage_beats_full_redraw();
    check_scroll_beats_repaint();
    check_sparse_history_storage();
    let single = measure_single_pane();
    let fleet = measure_fleet(45);

    println!("\n== budget check ==");
    let worst = single.max(fleet);
    let ok = worst < LATENCY_BUDGET_NS;
    println!(
        "worst p95 render: {:.3} us  (budget {:.0} us)  => {}",
        worst as f64 / 1000.0,
        LATENCY_BUDGET_NS as f64 / 1000.0,
        if ok { "PASS" } else { "FAIL" }
    );
    if !ok {
        std::process::exit(1);
    }
}

fn check_scroll_beats_repaint() {
    let mut grid = Grid::new(COLS, ROWS);
    grid.clear_damage();
    grid.scroll_up(Cell::default());
    for (x, ch) in "agent output settled".chars().enumerate() {
        grid.set(x as u16, ROWS - 1, cell(ch, 7));
    }

    let mut optimized = Vec::new();
    Renderer::new().render_pane_damage_with_scroll(&grid, 0, 0, &mut optimized);
    let mut fallback = Vec::new();
    Renderer::new().render_pane_damage(&grid, 0, 0, &mut fallback);
    println!(
        "[3] scroll diff {} bytes vs fallback {} bytes  ({}x smaller)",
        optimized.len(),
        fallback.len(),
        fallback.len().checked_div(optimized.len()).unwrap_or(0)
    );
    assert!(
        optimized.len() < fallback.len() / 10,
        "semantic scrolling must be at least 10x smaller"
    );
}

fn check_sparse_history_storage() {
    let mut grid = Grid::new(COLS, ROWS);
    for line in 0..10_000 {
        let text = format!("agent event {line}");
        for (x, ch) in text.chars().enumerate() {
            grid.set(x as u16, ROWS - 1, cell(ch, 7));
        }
        grid.scroll_up(Cell::default());
    }
    let dense = grid.total_lines() * usize::from(COLS);
    let retained = grid.retained_cell_count();
    let snapshot_cells: usize = grid
        .export_lines(1_000)
        .iter()
        .map(|line| line.cells.len())
        .sum();
    println!(
        "[4] sparse history retains {retained}/{dense} cells; recent snapshot {snapshot_cells}/{}",
        1_000 * usize::from(COLS)
    );
    assert!(
        retained < dense / 4,
        "sparse history must use less than 25% of dense cell storage"
    );
    assert!(
        snapshot_cells < 1_000 * usize::from(COLS) / 4,
        "snapshot projection must preserve the compact history shape"
    );
}

fn check_zero_output_when_idle() {
    let grid = Grid::new(COLS, ROWS);
    let mut r = Renderer::new();
    let mut out = Vec::new();
    let n = r.render(&grid, &mut out);
    println!("[1] idle grid render        -> {n} bytes  (expect 0)");
    assert_eq!(n, 0, "a clean grid must produce zero output");
}

fn check_damage_beats_full_redraw() {
    // Fill a grid, render once (this is the "full" cost), then change one cell
    // and render again (the "diff" cost). The diff must be dramatically smaller.
    let mut grid = Grid::new(COLS, ROWS);
    for y in 0..ROWS {
        for x in 0..COLS {
            grid.set(x, y, cell('.', 0));
        }
    }
    let mut r = Renderer::new();
    let mut full = Vec::new();
    let full_bytes = r.render(&grid, &mut full);
    grid.clear_damage();

    grid.set(10, 10, cell('X', 9));
    let mut diff = Vec::new();
    let diff_bytes = r.render(&grid, &mut diff);
    grid.clear_damage();

    println!(
        "[2] full redraw {full_bytes} bytes vs one-cell diff {diff_bytes} bytes  \
         ({}x smaller)",
        full_bytes.checked_div(diff_bytes).unwrap_or(0)
    );
    assert!(
        diff_bytes < full_bytes / 100,
        "damage tracking must be >100x smaller"
    );
}

fn measure_single_pane() -> u128 {
    let mut grid = Grid::new(COLS, ROWS);
    let mut r = Renderer::new();
    let mut rng = Rng(0x1234_5678);
    let mut out = Vec::with_capacity(64);
    let mut total_bytes = 0usize;

    let mut samples = Vec::with_capacity(FRAMES);
    for f in 0..FRAMES {
        let start = Instant::now();
        // Simulate a typical frame: a handful of cells change (a cursor blink,
        // a few characters typed, a status update).
        for _ in 0..4 {
            let x = rng.below(COLS);
            let y = rng.below(ROWS);
            grid.set(x, y, cell((b'a' + (f as u8 % 26)) as char, (f % 16) as u8));
        }
        out.clear();
        total_bytes += r.render(&grid, &mut out);
        grid.clear_damage();
        samples.push(start.elapsed().as_nanos());
    }
    samples.sort_unstable();
    let p95 = samples[FRAMES * 95 / 100];
    println!(
        "[5] 1 pane: p95 {:.3} us/frame, avg {} bytes/frame",
        p95 as f64 / 1000.0,
        total_bytes / FRAMES
    );
    p95
}

fn measure_fleet(panes: usize) -> u128 {
    // 45 panes, each smaller (a tiled layout). Each frame, a few panes change.
    let pw = COLS;
    let ph = ROWS / 4 + 1;
    let mut grids: Vec<Grid> = (0..panes).map(|_| Grid::new(pw, ph)).collect();
    let mut renderers: Vec<Renderer> = (0..panes).map(|_| Renderer::new()).collect();
    let mut rng = Rng(0xdead_beef);
    let mut out = Vec::with_capacity(64);

    let mut samples = Vec::with_capacity(FRAMES);
    for f in 0..FRAMES {
        let start = Instant::now();
        // Only a subset of panes produce output in a given frame - the realistic
        // case. Inactive panes cost nothing because their grids stay clean.
        for _ in 0..6 {
            let p = (rng.next() as usize) % panes;
            let x = rng.below(pw);
            let y = rng.below(ph);
            grids[p].set(x, y, cell((b'a' + (f as u8 % 26)) as char, (f % 16) as u8));
        }
        // Render every pane; clean ones short-circuit to zero work.
        for (g, r) in grids.iter_mut().zip(renderers.iter_mut()) {
            out.clear();
            r.render(g, &mut out);
            g.clear_damage();
        }
        samples.push(start.elapsed().as_nanos());
    }
    samples.sort_unstable();
    let p95 = samples[FRAMES * 95 / 100];
    println!(
        "[6] {panes} panes: p95 {:.3} us/frame (renders all panes; clean ones are free)",
        p95 as f64 / 1000.0
    );
    p95
}
