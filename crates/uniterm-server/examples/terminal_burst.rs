//! Release-mode parsing budget for real shell-output bursts, including retained history.
//! Run: `cargo run --release -p uniterm-server --example terminal_burst`.
//! This isolates PTY-byte parsing and grid work; it does not measure shell, I/O or rendering.

use std::fmt::Write;
use std::time::Instant;
use uniterm_server::terminal::Terminal;

const LINES: usize = 50_000;
const BURSTS: usize = 10;
const COLS: u16 = 160;
const ROWS: u16 = 50;

fn main() {
    let mut terminal = Terminal::new(COLS, ROWS);
    let mut samples = Vec::new();
    for burst in 0..BURSTS {
        let payload: String = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
            .iter()
            .cycle()
            .skip(burst)
            .take(62)
            .map(|b| char::from(*b))
            .collect();
        let mut bytes = String::with_capacity(LINES * 73);
        for line in 0..LINES {
            write!(bytes, "{line:08} {payload}\r\n").unwrap();
        }
        let start = Instant::now();
        for chunk in bytes.as_bytes().chunks(16_384) {
            terminal.feed(chunk);
        }
        let elapsed = start.elapsed();
        samples.push(elapsed.as_secs_f64() * 1_000.0);
        // Check the entire final screen and all retained history, outside the sample.
        assert_eq!(terminal.cursor(), (0, ROWS - 1));
        assert_eq!(terminal.grid().scrollback_len(), 10_000);
        let grid = terminal.grid();
        let total = grid.total_lines();
        for line in 0..total - 1 {
            let expected = format!("{:08} {payload}", LINES - (total - 1) + line);
            for (x, ch) in expected.chars().enumerate() {
                assert_eq!(grid.line_cell(line, x as u16).ch, ch);
            }
            for x in expected.len() as u16..COLS {
                assert_eq!(grid.line_cell(line, x).ch, ' ');
            }
            assert!(!grid.line_wrapped(line));
        }
        assert!((0..COLS).all(|x| grid.get(x, ROWS - 1).ch == ' '));
        terminal.grid_mut().clear_damage();
        terminal.feed(&[]);
        assert!(!terminal.grid().is_dirty());
        println!("burst {burst}: {:.3} ms", elapsed.as_secs_f64() * 1_000.0);
    }
    samples.sort_by(f64::total_cmp);
    let median = (samples[4] + samples[5]) / 2.0;
    println!("{LINES} lines, {COLS}x{ROWS}, {BURSTS} bursts: median {median:.3} ms");
    // Reserve most of the end-to-end target for shell production, I/O and rendering.
    assert!(
        median < 100.0,
        "parser burst budget exceeded: {median:.3} ms"
    );
}
