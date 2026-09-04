//! Differential VT conformance against tmux's mature screen model.
//!
//! This runs only when tmux is available. The fixture combines cursor edits,
//! erasure, SGR, combining text, and a double-width glyph, then compares the
//! physical viewport row for row.

use std::process::Command;
use std::thread;
use std::time::Duration;

use uniterm_server::Terminal;

fn normalized(text: &str, height: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .take(height)
        .map(|line| line.trim_end().to_string())
        .collect();
    lines.resize(height, String::new());
    lines
}

#[test]
fn common_cli_stream_matches_tmux_viewport() {
    if Command::new("tmux").arg("-V").output().is_err() {
        return;
    }

    let width = 24u16;
    let height = 5u16;
    let fixture = b"hello world\x1b[5DTERM\r\n\x1b[1;38;2;9;8;7mwide:\x1b[0m \xe7\x95\x8c e\xcc\x81\r\nthird line\x1b[4D\x1b[2P!!\r\nclear-me\x1b[2KOK";
    let unique = format!(
        "{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("vt")
    );
    let socket = format!("uniterm-vt-{unique}");
    let path = std::env::temp_dir().join(format!("uniterm-vt-{unique}.ansi"));
    std::fs::write(&path, fixture).unwrap();
    let shell = format!("sleep 0.2; cat {}; sleep 0.8", path.display());

    let status = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-x",
            &width.to_string(),
            "-y",
            &(height + 1).to_string(),
            &shell,
        ])
        .status()
        .unwrap();
    assert!(status.success());
    let _ = Command::new("tmux")
        .args(["-L", &socket, "set-option", "status", "off"])
        .status();
    let _ = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "resize-window",
            "-x",
            &width.to_string(),
            "-y",
            &height.to_string(),
        ])
        .status();
    thread::sleep(Duration::from_millis(450));
    let capture = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "capture-pane",
            "-p",
            "-S",
            "0",
            "-E",
            &(height - 1).to_string(),
        ])
        .output()
        .unwrap();
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .status();
    let _ = std::fs::remove_file(path);
    assert!(capture.status.success());

    let mut uniterm = Terminal::new(width, height);
    uniterm.feed(fixture);
    let expected = normalized(&String::from_utf8_lossy(&capture.stdout), height as usize);
    let actual = normalized(&uniterm.dump_text(), height as usize);
    assert_eq!(actual, expected);
}
