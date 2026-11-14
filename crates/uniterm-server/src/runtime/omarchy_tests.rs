//! Native-watch regressions use only disposable palette trees, never a Workspace.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use uniterm_core::Theme;

const PALETTE: &str = "background = \"#101010\"\nforeground = \"#eeeeee\"\naccent = \"#5588ff\"\ncolor1 = \"#ff4444\"\ncolor2 = \"#44ff44\"\ncolor3 = \"#ffff44\"\n";

fn temp_root() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "uniterm-omarchy-watch-{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ))
}

struct Fixture {
    service: Service,
    root: PathBuf,
    poll: Poll,
    waker: Arc<Waker>,
    tx: Sender<AgentToCore>,
    rx: Receiver<AgentToCore>,
}

impl Fixture {
    fn new() -> Self {
        let root = temp_root();
        std::fs::create_dir_all(root.join("theme")).unwrap();
        std::fs::write(root.join("theme/colors.toml"), PALETTE).unwrap();
        let poll = Poll::new().unwrap();
        let waker = Arc::new(Waker::new(poll.registry(), Token(701)).unwrap());
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            service: Service {
                host: Some(root.clone()),
                task: None,
                serial: None,
            },
            root,
            poll,
            waker,
            tx,
            rx,
        }
    }

    fn configure(&mut self, enabled: bool, generation: u64) {
        self.service
            .configure(enabled, generation, self.tx.clone(), self.waker.clone());
    }

    async fn reply(&mut self) -> (u64, Option<Theme>, Option<String>) {
        let rx = self.rx.clone();
        let reply = tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(3)))
            .await
            .unwrap()
            .expect("watcher must report an observation");
        let AgentToCore::OmarchyTheme {
            generation,
            theme,
            error,
        } = reply
        else {
            panic!("unexpected runtime reply");
        };
        self.poll_events(Duration::from_secs(1));
        (generation, theme, error)
    }

    fn poll_events(&mut self, duration: Duration) -> Events {
        let deadline = Instant::now() + duration;
        let mut events = Events::with_capacity(4);
        loop {
            match self.poll.poll(
                &mut events,
                Some(deadline.saturating_duration_since(Instant::now())),
            ) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => {
                    result.unwrap();
                    return events;
                }
            }
        }
    }

    fn quiet(&mut self, duration: Duration) {
        let events = self.poll_events(duration);
        assert!(events.is_empty(), "unchanged palette woke the core");
        assert!(
            self.rx.try_recv().is_err(),
            "unchanged palette sent a reply"
        );
    }

    fn write(&self, text: &str) {
        std::fs::write(self.root.join("theme/colors.toml"), text).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.service
            .configure(false, 0, self.tx.clone(), self.waker.clone());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_palette_watch_publishes_once_and_ignores_reads_background_and_unchanged_writes() {
    let mut fixture = Fixture::new();
    fixture.configure(true, 1);
    assert_eq!(
        fixture.reply().await,
        (1, Some(Theme::from_omarchy_colors(PALETTE).unwrap()), None)
    );
    fixture.quiet(Duration::from_millis(200));
    std::fs::read(fixture.root.join("theme/colors.toml")).unwrap();
    std::fs::write(fixture.root.join("background"), "wallpaper").unwrap();
    std::fs::write(fixture.root.join("theme/background.png"), "unrelated").unwrap();
    fixture.write(PALETTE);
    fixture.quiet(Duration::from_millis(350));
    let changed = PALETTE.replace("#5588ff", "#dd8844");
    fixture.write(&changed);
    assert_eq!(
        fixture.reply().await,
        (1, Some(Theme::from_omarchy_colors(&changed).unwrap()), None)
    );
    fixture.quiet(Duration::from_millis(200));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_palette_watch_rearms_replaced_theme_and_recovers_from_bad_files() {
    let mut fixture = Fixture::new();
    fixture.configure(true, 7);
    fixture.reply().await;
    let changed = PALETTE.replace("#101010", "#303030");
    let staging = fixture.root.join("next-theme");
    std::fs::create_dir(&staging).unwrap();
    std::fs::write(staging.join("colors.toml"), &changed).unwrap();
    std::fs::remove_dir_all(fixture.root.join("theme")).unwrap();
    std::fs::rename(staging, fixture.root.join("theme")).unwrap();
    std::fs::write(fixture.root.join("theme.name"), "next").unwrap();
    assert_eq!(
        fixture.reply().await,
        (7, Some(Theme::from_omarchy_colors(&changed).unwrap()), None)
    );
    for invalid in ["background = \"#12", &"#".repeat(65 * 1024)] {
        fixture.write(invalid);
        let (generation, theme, error) = fixture.reply().await;
        assert_eq!(generation, 7);
        assert!(theme.is_none() && error.is_some());
        fixture.quiet(Duration::from_millis(180));
        fixture.write(&changed);
        assert_eq!(
            fixture.reply().await,
            (7, Some(Theme::from_omarchy_colors(&changed).unwrap()), None)
        );
    }
    let path = fixture.root.join("theme/colors.toml");
    std::fs::remove_file(&path).unwrap();
    let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: the CString is NUL-terminated and lives across the syscall; this
    // creates only a fixture-owned FIFO so an untrusted source cannot block reads.
    assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
    let (_, theme, error) = fixture.reply().await;
    assert!(theme.is_none() && error.is_some());
    fixture.quiet(Duration::from_millis(180));
    std::fs::remove_file(&path).unwrap();
    fixture.write(PALETTE);
    assert_eq!(
        fixture.reply().await,
        (7, Some(Theme::from_omarchy_colors(PALETTE).unwrap()), None)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_palette_watch_recovers_when_theme_is_absent_at_first_attach() {
    let mut fixture = Fixture::new();
    std::fs::remove_dir_all(fixture.root.join("theme")).unwrap();
    fixture.configure(true, 9);
    let (generation, theme, error) = fixture.reply().await;
    assert_eq!(generation, 9);
    assert!(theme.is_none() && error.is_some());
    fixture.quiet(Duration::from_millis(180));
    std::fs::create_dir(fixture.root.join("theme")).unwrap();
    fixture.write(PALETTE);
    assert_eq!(
        fixture.reply().await,
        (9, Some(Theme::from_omarchy_colors(PALETTE).unwrap()), None)
    );
    fixture.quiet(Duration::from_millis(180));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_palette_watch_disable_reenable_and_ineligible_hosts_stay_idle() {
    let mut fixture = Fixture::new();
    let mut unavailable = Service::default();
    unavailable.configure(true, 1, fixture.tx.clone(), fixture.waker.clone());
    assert!(!unavailable.available());
    assert!(unavailable.task.is_none());
    fixture.quiet(Duration::from_millis(100));
    fixture.configure(true, 20);
    assert_eq!(fixture.reply().await.0, 20);
    fixture.configure(false, 21);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        Arc::strong_count(&fixture.waker),
        1,
        "disabled task retained the core waker"
    );
    fixture.write(&PALETTE.replace("#5588ff", "#112233"));
    fixture.quiet(Duration::from_millis(200));
    fixture.configure(true, 22);
    let (generation, theme, error) = fixture.reply().await;
    assert_eq!(generation, 22);
    assert_eq!(theme.unwrap().accent, uniterm_core::Color::Rgb(17, 34, 51));
    assert!(error.is_none());
    let service = std::mem::take(&mut fixture.service);
    drop(service);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        Arc::strong_count(&fixture.waker),
        1,
        "dropped task retained the core waker"
    );
    fixture.write(PALETTE);
    fixture.quiet(Duration::from_millis(200));
}

fn usage_seconds() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage initializes the valid out pointer on success; no shared state is accessed.
    assert_eq!(
        unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
        0
    );
    // SAFETY: the successful syscall initialized the complete rusage value.
    let usage = unsafe { usage.assume_init() };
    usage.ru_utime.tv_sec as f64
        + usage.ru_stime.tv_sec as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
}

fn process_resources() -> (u64, u64, usize) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let number = |key: &str| -> u64 {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.split_whitespace().next())
            .unwrap()
            .parse()
            .unwrap()
    };
    (
        number("VmRSS:"),
        number("Threads:"),
        std::fs::read_dir("/proc/self/fd").unwrap().count(),
    )
}

#[test]
fn omarchy_idle_worker() {
    let Some(output) = std::env::var_os("UNITERM_TEST_OMARCHY_IDLE_OUTPUT") else {
        return;
    };
    let enabled = std::env::var("UNITERM_TEST_OMARCHY_IDLE_ENABLED").unwrap() == "true";
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut fixture = runtime.block_on(async {
        tokio::task::spawn_blocking(|| ()).await.unwrap();
        let mut fixture = Fixture::new();
        fixture.configure(enabled, 1);
        if enabled {
            fixture.reply().await;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        fixture
    });
    let start_cpu = usage_seconds();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(300);
    let mut events = Events::with_capacity(4);
    loop {
        match fixture.poll.poll(
            &mut events,
            Some(deadline.saturating_duration_since(Instant::now())),
        ) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => {
                result.unwrap();
                break;
            }
        }
    }
    let cpu_seconds = usage_seconds() - start_cpu;
    let elapsed_seconds = started.elapsed().as_secs_f64();
    let messages = fixture.rx.try_iter().count();
    let (rss_kib, threads, fds) = process_resources();
    let report = serde_json::json!({
        "enabled": enabled, "elapsed_seconds": elapsed_seconds,
        "cpu_seconds": cpu_seconds, "cpu_percent": cpu_seconds / elapsed_seconds * 100.0,
        "rss_kib": rss_kib, "threads": threads, "fds": fds,
        "wakeups": events.iter().count(), "messages": messages,
    });
    std::fs::write(output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    drop(fixture);
    drop(runtime);
}

#[test]
#[ignore = "explicit five-minute native watcher CPU, wakeup and memory qualification"]
fn omarchy_following_five_minute_idle_budget() {
    let root = temp_root();
    std::fs::create_dir_all(&root).unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for enabled in [false, true] {
        let output = root.join(format!("{enabled}.json"));
        let child = std::process::Command::new(&exe)
            .args([
                "--exact",
                "runtime::omarchy::tests::omarchy_idle_worker",
                "--nocapture",
            ])
            .env("UNITERM_TEST_OMARCHY_IDLE_OUTPUT", &output)
            .env("UNITERM_TEST_OMARCHY_IDLE_ENABLED", enabled.to_string())
            .spawn()
            .unwrap();
        children.push((child, output));
    }
    let mut reports = Vec::new();
    for (mut child, output) in children {
        assert!(child.wait().unwrap().success());
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
        eprintln!("Omarchy idle qualification: {report}");
        assert!(report["elapsed_seconds"].as_f64().unwrap() >= 299.0);
        assert!(report["cpu_percent"].as_f64().unwrap() < 0.5);
        assert_eq!(report["wakeups"], 0);
        assert_eq!(report["messages"], 0);
        reports.push(report);
    }
    let delta = |key: &str| reports[1][key].as_i64().unwrap() - reports[0][key].as_i64().unwrap();
    eprintln!(
        "Omarchy incremental idle resources: RSS={} KiB, threads={}, fds={}",
        delta("rss_kib"),
        delta("threads"),
        delta("fds")
    );
    assert!(
        delta("threads") <= 1,
        "one native watcher thread is the maximum"
    );
    assert!(delta("fds") <= 4, "native watching must remain bounded");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn native_watch_registration_keeps_pre_registration_identity_on_replacement() {
    use notify::Watcher;
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new();
    let directory = fixture.root.join("theme");
    let original = std::fs::metadata(&directory).unwrap();
    let replacement = fixture.root.join("replacement");
    std::fs::create_dir(&replacement).unwrap();
    let mut watcher = notify::INotifyWatcher::new(|_| {}, notify::Config::default()).unwrap();
    let recorded = linux::watch_directory(&directory, |path| {
        watcher
            .watch(path, notify::RecursiveMode::NonRecursive)
            .unwrap();
        std::fs::rename(path, fixture.root.join("old-theme")).unwrap();
        std::fs::rename(&replacement, path).unwrap();
        Ok(())
    })
    .unwrap();
    let current = std::fs::metadata(&directory).unwrap();
    assert_eq!(recorded, Some((original.dev(), original.ino())));
    assert_ne!(
        recorded,
        Some((current.dev(), current.ino())),
        "the next observation must rearm the replaced directory"
    );
}

#[test]
fn native_watch_fatal_callback_survives_a_full_coalescing_channel() {
    let signal = linux::EventSignal::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    signal.signal(&tx, false);
    signal.signal(&tx, true);
    signal.signal(&tx, false);
    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_err());
    assert!(
        signal.failure().is_some(),
        "fatal state must outlive its dropped wake event"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_watch_canceled_observers_wait_for_previous_blocking_work() {
    let mut fixture = Fixture::new();
    let serial = Arc::new(tokio::sync::Semaphore::new(1));
    fixture.service.serial = Some(serial.clone());
    let held = serial.clone().acquire_owned().await.unwrap();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    // Model a previous observer's detached blocking I/O. Its permit remains
    // owned by that operation even after the observing async task is canceled.
    let blocked = tokio::task::spawn_blocking(move || {
        let _permit = held;
        release_rx.recv().unwrap();
    });
    for generation in 1..=100 {
        fixture.configure(true, generation);
        tokio::task::yield_now().await;
    }
    fixture.quiet(Duration::from_millis(100));
    assert_eq!(serial.available_permits(), 0);
    release_tx.send(()).unwrap();
    blocked.await.unwrap();
    assert_eq!(fixture.reply().await.0, 100);
    assert_eq!(
        serial.available_permits(),
        0,
        "active observer must keep its lease"
    );
    let task = fixture.service.task.take().unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let released = tokio::time::timeout(Duration::from_secs(3), serial.clone().acquire_owned())
        .await
        .unwrap()
        .unwrap();
    drop(released);
    assert_eq!(
        serial.available_permits(),
        1,
        "stopped observer must release its lease"
    );
    fixture.quiet(Duration::from_millis(100));
}

#[test]
fn native_watch_child_missing_during_registration_keeps_parent_recoverable() {
    let fixture = Fixture::new();
    let directory = fixture.root.join("theme");
    for error in [
        notify::Error::path_not_found(),
        notify::Error::from(std::io::Error::from(std::io::ErrorKind::NotFound)),
    ] {
        let mut error = Some(error);
        assert_eq!(
            linux::watch_directory(&directory, |_| Err(error.take().unwrap())).unwrap(),
            None
        );
    }
    assert!(
        linux::watch_directory(&directory, |_| Err(notify::Error::from(
            std::io::Error::from(std::io::ErrorKind::PermissionDenied)
        )))
        .is_err()
    );
}
