//! Debug helper: feed a raw byte capture through the emulator and dump the grid.
//! `cargo run -p uniterm-server --example emu_file -- <file> [cols rows]`
use uniterm_server::Terminal;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: emu_file <file> [cols rows]");
    let cols: u16 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100);
    let rows: u16 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let bytes = std::fs::read(&path).expect("read file");
    let mut t = Terminal::new(cols, rows);
    t.feed(&bytes);
    println!("--- reconstructed grid ---");
    print!("{}", t.dump_text());
    println!("--- end ---");
    println!("cursor: {:?}", t.cursor());
}
