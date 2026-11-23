//! Project-only label alignment through Settings, first attach, and saved config.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};
use uniterm_core::{Config, TabLabelAlignment};
use uniterm_proto::{
    encode_frame, ClientMessage, Command, FrameDecoder, MouseKind, ServerMessage, SettingsPatch,
    SettingsSnapshot,
};
use uniterm_server::{Server, Terminal};

mod common;

struct Client {
    stream: UnixStream,
    decoder: FrameDecoder,
    screen: Terminal,
    render_bytes: usize,
    settings: Option<(SettingsSnapshot, bool, Option<String>)>,
}

impl Client {
    fn attach(socket: &Path) -> Self {
        common::wait_for_socket(socket);
        let stream = UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut client = Self {
            stream,
            decoder: FrameDecoder::new(),
            screen: Terminal::new(120, 24),
            render_bytes: 0,
            settings: None,
        };
        client.send(ClientMessage::Attach {
            term: "xterm-256color".into(),
            cols: 120,
            rows: 24,
        });
        client.until(|client| client.settings.is_some() && client.render_bytes > 0);
        client
    }

    fn send(&mut self, message: ClientMessage) {
        self.stream.write_all(&encode_frame(&message)).unwrap();
    }

    fn read(&mut self) {
        let mut bytes = [0; 32768];
        match self.stream.read(&mut bytes) {
            Ok(0) => panic!("server disconnected"),
            Ok(n) => self.decoder.push(&bytes[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("{error}"),
        }
        while let Some(message) = self.decoder.decode::<ServerMessage>().unwrap() {
            match message {
                ServerMessage::RenderOps(ops) => {
                    self.render_bytes += ops.len();
                    self.screen.feed(&ops);
                }
                ServerMessage::Settings {
                    settings,
                    saved,
                    error,
                } => {
                    self.settings = Some((*settings, saved, error));
                }
                _ => {}
            }
        }
    }

    fn until(&mut self, ready: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready(self) {
            assert!(
                Instant::now() < deadline,
                "screen: {}",
                self.screen.dump_text()
            );
            self.read();
        }
    }

    fn apply(&mut self, value: &str) {
        self.settings = None;
        self.send(ClientMessage::SettingsApply(SettingsPatch {
            tab_label_alignment: Some(value.into()),
            ..Default::default()
        }));
        self.until(|client| client.settings.is_some());
    }

    fn click(&mut self, x: u16, y: u16) {
        self.send(ClientMessage::Mouse {
            x: x + 1,
            y: y + 1,
            kind: MouseKind::Click,
        });
    }

    fn row(&self, x: u16, y: u16, width: u16) -> String {
        (x..x + width)
            .map(|x| self.screen.grid().get(x, y).ch)
            .collect()
    }
}

fn start(socket: &Path, config: Config) -> std::thread::JoinHandle<()> {
    let socket = socket.to_owned();
    std::thread::spawn(move || {
        let (mut server, mut poll) = Server::bind(&socket, "/bin/cat", &[], 120, 24).unwrap();
        server.set_config(config);
        server.run(&mut poll).unwrap();
    })
}

fn aligned(label: &str, width: usize, alignment: TabLabelAlignment) -> String {
    let remaining = width - label.len();
    let leading = match alignment {
        TabLabelAlignment::Left => 0,
        TabLabelAlignment::Center => remaining / 2,
        TabLabelAlignment::Right => remaining,
    };
    format!(
        "{}{}{}",
        " ".repeat(leading),
        label,
        " ".repeat(remaining - leading)
    )
}

#[test]
fn project_tab_alignment_settings_persist_attach_and_preserve_geometry() {
    common::isolate_state();
    let directory = common::temp_dir("tab-align");
    std::env::set_var("XDG_CONFIG_HOME", directory.join("config"));
    std::env::set_var("XDG_RUNTIME_DIR", &directory);
    let config_path = directory.join("config/uniterm/uniterm.conf");
    let sockets =
        common::socket_root().join(format!("ut-align-{}", common::unique_workspace_name()));
    std::fs::create_dir_all(&sockets).unwrap();
    let socket = sockets.join(format!("{}.sock", common::unique_workspace_name()));
    let server = start(&socket, Config::default());
    let mut client = Client::attach(&socket);
    assert_eq!(
        client.settings.as_ref().unwrap().0.tab_label_alignment,
        "center"
    );
    client.send(ClientMessage::RenameWindow {
        name: "Root".into(),
    });
    client.send(ClientMessage::Command(Command::NewStackTab));
    client.send(ClientMessage::RenameWindow {
        name: "Child".into(),
    });
    client.until(|client| client.row(24, 1, 60).contains("2:Child"));

    // The unchanged 60-cell center area has a 57-cell root slot, two
    // 29/28-cell stack slots, and a three-cell plus control in each row.
    let outside = |client: &Client| {
        (0..24)
            .flat_map(|y| {
                (0..120).filter_map(move |x| {
                    (!(24..81).contains(&x) || y > 1).then_some(client.screen.grid().get(x, y))
                })
            })
            .collect::<Vec<_>>()
    };
    let original_outside = outside(&client);
    let root_style = client.screen.grid().get(24, 0);
    let inactive_style = client.screen.grid().get(24, 1);
    let active_style = client.screen.grid().get(53, 1);
    let mut saved_config = Config::default();
    for alignment in [
        TabLabelAlignment::Center,
        TabLabelAlignment::Left,
        TabLabelAlignment::Right,
    ] {
        client.apply(alignment.name());
        let (settings, saved, error) = client.settings.as_ref().unwrap();
        assert_eq!(settings.tab_label_alignment, alignment.name());
        assert!(*saved, "config was not saved: {error:?}");
        assert!(error.is_none());
        client.until(|client| {
            client.row(24, 0, 57) == aligned("1:Root", 57, alignment)
                && client.row(24, 1, 29) == aligned("1:Root", 29, alignment)
                && client.row(53, 1, 28) == aligned("2:Child", 28, alignment)
        });
        assert_eq!(
            outside(&client),
            original_outside,
            "unrelated cells changed"
        );
        for (y, x, width, style) in [
            (0, 24, 57, root_style),
            (1, 24, 29, inactive_style),
            (1, 53, 28, active_style),
        ] {
            for col in x..x + width {
                let cell = client.screen.grid().get(col, y);
                assert_eq!(
                    (cell.fg, cell.bg, cell.attrs),
                    (style.fg, style.bg, style.attrs)
                );
            }
        }
        saved_config = Config::parse(&std::fs::read_to_string(&config_path).unwrap());
        assert_eq!(saved_config.tab_label_alignment, alignment);

        let mut second = Client::attach(&socket);
        assert_eq!(
            second.settings.as_ref().unwrap().0.tab_label_alignment,
            alignment.name()
        );
        second.until(|second| second.row(24, 1, 57) == client.row(24, 1, 57));
        assert_eq!(second.row(24, 0, 60), client.row(24, 0, 60));
        drop(second);

        // Click the same slot edges regardless of where their text landed.
        client.click(24, 1);
        client.until(|client| client.screen.grid().get(24, 1).bg == active_style.bg);
        client.click(80, 1);
        client.until(|client| client.screen.grid().get(53, 1).bg == active_style.bg);
        assert_eq!(outside(&client), original_outside);

        // Consume the asynchronous save acknowledgement and prove no render
        // bytes arrive for selecting the current choice or subsequent idle.
        let bytes = client.render_bytes;
        client.apply(alignment.name());
        assert_eq!(
            client.render_bytes, bytes,
            "no-op setting emitted RenderOps"
        );
        client.read();
        assert_eq!(client.render_bytes, bytes, "no-op armed a delayed repaint");
    }
    let bytes = client.render_bytes;
    client.apply("middle");
    let (settings, saved, error) = client.settings.as_ref().unwrap();
    assert_eq!(settings.tab_label_alignment, "right");
    assert!(!saved && error.is_some());
    assert_eq!(client.render_bytes, bytes);
    assert_eq!(
        Config::parse(&std::fs::read_to_string(&config_path).unwrap()),
        saved_config
    );

    // Fixed plus targets still create one child and one root Tab.
    client.click(82, 1);
    client.until(|client| client.row(24, 1, 60).contains("3:Tab"));
    client.click(82, 0);
    client.until(|client| client.row(24, 0, 60).contains('4'));
    // Root selection still owns the entire rectangle, including its padding.
    client.click(24, 0);
    client.until(|client| client.row(24, 1, 60).contains("2:Child"));

    // Both root and child labels use the same narrow grapheme/ellipsis rule.
    let unicode_name = "界e\u{301}👩‍💻界界界界界界界界界界界界界界界界界界界界界界";
    client.send(ClientMessage::Command(Command::SelectWindow(0)));
    client.send(ClientMessage::RenameWindow {
        name: unicode_name.into(),
    });
    client.send(ClientMessage::Command(Command::SelectWindow(1)));
    client.send(ClientMessage::RenameWindow {
        name: unicode_name.into(),
    });
    client
        .until(|client| client.row(24, 0, 29).contains('…') && client.row(43, 1, 19).contains('…'));
    for alignment in TabLabelAlignment::ALL {
        client.apply(alignment.name());
        client.until(|client| client.row(24, 0, 29).contains('…'));
        // Grapheme clusters remain intact in the terminal's cell storage.
        for y in [0, 1] {
            assert!(client.row(24, y, 57).contains('界'));
            assert!(client.row(24, y, 57).contains('…'));
            let grid = client.screen.grid();
            let cells: Vec<_> = (24..81)
                .map(|x| grid.cell_text_owned(grid.get(x, y)))
                .collect();
            assert!(cells.iter().any(|text| text == "e\u{301}"));
            assert!(cells.iter().any(|text| text == "👩‍💻"));
        }
        assert_eq!(client.row(81, 0, 3), " + ");
        assert_eq!(client.row(81, 1, 3), " + ");
    }
    client.send(ClientMessage::KillServer);
    server.join().unwrap();
    drop(client);

    // Load the actual persisted setting for another server's first attach.
    let persisted = Config::parse(&std::fs::read_to_string(&config_path).unwrap());
    assert_eq!(persisted.tab_label_alignment, TabLabelAlignment::Right);
    let restored_socket = sockets.join(format!("{}.sock", common::unique_workspace_name()));
    let restored = start(&restored_socket, persisted);
    let mut attached = Client::attach(&restored_socket);
    assert_eq!(
        attached.settings.as_ref().unwrap().0.tab_label_alignment,
        "right"
    );
    attached.send(ClientMessage::RenameWindow {
        name: "Saved".into(),
    });
    attached
        .until(|client| client.row(24, 0, 57) == aligned("1:Saved", 57, TabLabelAlignment::Right));
    attached.send(ClientMessage::KillServer);
    restored.join().unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(sockets).unwrap();
}
