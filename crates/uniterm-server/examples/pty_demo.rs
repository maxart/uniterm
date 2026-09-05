//! Phase 1 M1 demo: run a real process on a PTY, feed its output through the
//! VT emulator into a grid, and reconstruct the screen from that grid.
//!
//! Run: `cargo run -p uniterm-server --example pty_demo`
//!
//! This proves the whole M1 pipeline end to end: PTY -> vte parse -> our grid
//! (with damage) -> reconstructed text + a minimal damage render.

use uniterm_server::pty::PtyProcess;
use uniterm_server::{Renderer, Terminal};

fn main() -> std::io::Result<()> {
    println!("== Uniterm M1 pipeline demo ==\n");

    let (cols, rows) = (80u16, 24u16);
    let mut term = Terminal::new(cols, rows);

    // A deterministic command exercising print, colour SGR, CR/LF, and cursor
    // addressing - the common shapes real shells emit.
    let script = "printf 'Uniterm multiplexer - M1\\n'; \
                  printf '\\033[1;32mgreen bold\\033[0m and \\033[31mred\\033[0m\\n'; \
                  printf 'tab\\tstops\\n'; \
                  printf '\\033[5;40Hpositioned at row5 col40\\n'";
    let mut child = PtyProcess::spawn("/bin/sh", &["-c", script], cols, rows, None, &[])?;

    // Blocking read to EOF (M2 makes this event-driven off mio).
    let mut buf = [0u8; 4096];
    let mut total = 0usize;
    loop {
        match child.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                term.feed(&buf[..n]);
                total += n;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break, // EIO on final close is normal for a PTY master
        }
    }
    let _ = child.wait();

    println!("fed {total} bytes from the child through the emulator\n");
    println!("--- reconstructed screen (from our grid) ---");
    print!("{}", term.dump_text());
    println!("--------------------------------------------\n");

    // Show the damage-tracked render path on real content.
    let mut r = Renderer::new();
    let mut out = Vec::new();
    let bytes = r.render(term.grid(), &mut out);
    term.grid_mut().clear_damage();
    println!("damage render of the populated grid: {bytes} bytes");

    // And the budget property: a second render with no changes emits nothing.
    let idle = r.render(term.grid(), &mut out);
    println!("second render with no changes: {idle} bytes (expect 0)");
    assert_eq!(idle, 0);

    println!("\nOK - PTY -> parse -> grid -> render verified");
    Ok(())
}
