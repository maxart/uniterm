//! Required external-oracle corpus in CI. Locally opt in with --ignored;
//! an unavailable tmux is then a failure, never a successful zero-assertion test.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthStr as _;
use uniterm_server::Terminal;

mod common;

struct Tmux {
    socket: PathBuf,
    root: PathBuf,
}

impl Tmux {
    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command
            .arg("-S")
            .arg(&self.socket)
            .args(["-f", "/dev/null"]);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self
            .command()
            .args(args)
            .output()
            .expect("tmux is required for the differential corpus");
        assert!(
            output.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

impl Drop for Tmux {
    fn drop(&mut self) {
        let _ = self.command().arg("kill-server").output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn normalized(text: &str, height: usize) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .take(height)
        .map(|line| {
            // tmux preserves literal tabs in capture-pane output. Compare
            // display columns, not that transport shorthand (default stops).
            let mut expanded = String::new();
            for (index, part) in line.split('\t').enumerate() {
                if index != 0 {
                    expanded.extend(std::iter::repeat_n(' ', 8 - expanded.width() % 8));
                }
                expanded.push_str(part);
            }
            expanded.trim_end().to_string()
        })
        .collect();
    lines.resize(height, String::new());
    lines
}

#[test]
#[ignore = "requires tmux; run explicitly with --ignored, as CI does"]
fn common_cli_stream_matches_tmux_viewport() {
    common::isolate_state();
    let version = Command::new("tmux")
        .arg("-V")
        .output()
        .expect("install tmux to run this corpus");
    assert!(version.status.success(), "tmux -V failed");
    let fixtures = [
        ("edits", "hello world\x1b[5DTERM\r\n\x1b[1;38;2;9;8;7mwide:\x1b[0m 界 e\u{301}\r\nthird line\x1b[4D\x1b[2P!!\r\nclear-me\x1b[2KOK"),
        ("wrap", "123456789012345678901234WRAPPED\r\nnext\r\nline3\r\nline4\r\nline5\r\nline6\r\nline7\r\nline8\r\nline9"),
        ("margins", "top\x1b[2;6r\x1b[2;1Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\x1b[2;1H\x1b[M\x1b[r"),
        ("alternate", "primary 界\x1b[?1049hALT\r\nscreen\x1b[2J\x1b[?1049l\x1b[2;1Hrestored"),
        // Destructive half-wide edits have different terminal policies;
        // local grid invariants cover those. Compare whole glyph edits here.
        ("wide-erase", "A界B e\u{301}\x1b[1;2H\x1b[2X\x1b[2;1H界界XX\x1b[2;1H\x1b[2P\x1b[3;1Htail\x1b[3;2H\x1b[2@++"),
        ("tabs-saved-cursor", "a\tb\tc\x1b7\x1b[4;5Haway\x1b8Z\r\nlast\x1b[1K"),
    ];
    for (index, (name, fixture)) in fixtures.into_iter().enumerate() {
        let root = common::socket_root().join(format!("ut-vt-{}-{index}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let tmux = Tmux {
            socket: root.join("oracle.sock"),
            root,
        };
        let path = tmux.root.join("fixture.ansi");
        let fixture = format!("\x1b[2J\x1b[H{fixture}\x1b[0m\x1b[r\x1b[8;1H__DONE__");
        std::fs::write(&path, fixture.as_bytes()).unwrap();
        // No startup sleep: fixture output is released only after tmux's
        // viewport has its final geometry and status setting.
        let shell = format!(
            "tmux -S '{}' wait-for start; cat '{}'; tmux -S '{}' wait-for hold",
            tmux.socket.display(),
            path.display(),
            tmux.socket.display()
        );
        tmux.run(&[
            "new-session",
            "-d",
            "-s",
            &common::unique_workspace_name(),
            "-x",
            "24",
            "-y",
            "9",
            &shell,
        ]);
        tmux.run(&["set-option", "status", "off"]);
        tmux.run(&["resize-window", "-x", "24", "-y", "8"]);
        tmux.run(&["wait-for", "-S", "start"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        let expected = loop {
            let capture = tmux.run(&["capture-pane", "-p", "-S", "0", "-E", "7"]);
            let text = String::from_utf8(capture.stdout).unwrap();
            if text.contains("__DONE__") {
                break normalized(&text, 8);
            }
            assert!(Instant::now() < deadline, "{name}: fixture never completed");
            thread::sleep(Duration::from_millis(10));
        };
        for chunk in [1, 2, 7, 4096] {
            let mut terminal = Terminal::new(24, 8);
            for bytes in fixture.as_bytes().chunks(chunk) {
                terminal.feed(bytes);
            }
            assert_eq!(
                normalized(&terminal.dump_text(), 8),
                expected,
                "{name}, chunk size {chunk}"
            );
        }
    }
}
