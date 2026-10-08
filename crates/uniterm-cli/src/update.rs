//! Explicit, checksum-verified replacement of the installed binary pair.
//! Network and disk work runs only in this one-shot CLI, never in a Workspace.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use sha2::{Digest, Sha256};

const REPO: &str = "maxart/uniterm";
const MAX_METADATA: u64 = 1_048_576;
const MAX_BINARY: u64 = 134_217_728;

#[derive(Default)]
struct Options {
    check: bool,
    json: bool,
    version: Option<String>,
}

#[derive(Serialize)]
struct Outcome {
    current: String,
    latest: String,
    update_available: bool,
    installed: bool,
    install_dir: PathBuf,
    message: String,
}

fn version(value: &str) -> Option<(u64, u64, u64)> {
    let mut fields = value.strip_prefix('v').unwrap_or(value).split('.');
    let major = fields.next()?.parse().ok()?;
    let minor = fields.next()?.parse().ok()?;
    let patch = fields.next()?.parse().ok()?;
    fields.next().is_none().then_some((major, minor, patch))
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut result = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => result.check = true,
            "--json" => result.json = true,
            "--version" if result.version.is_none() => {
                let value = args.next().ok_or("--version needs a release number")?;
                let (a, b, c) =
                    version(value).ok_or("version must have the form 1.2.1 or v1.2.1")?;
                result.version = Some(format!("v{a}.{b}.{c}"));
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    Ok(result)
}

fn platform() -> io::Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("macos-arm64"),
        ("android", "aarch64") => Ok("android-aarch64"),
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Ok("linux-x86_64"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Ok("linux-aarch64"),
        _ => Err(io::Error::other(
            "no prebuilt release for this platform; update from source",
        )),
    }
}

trait Download {
    fn fetch(&self, url: &str, path: &Path, limit: u64) -> io::Result<()>;
    fn resolve(&self, url: &str) -> io::Result<String>;
}
struct Https;
impl Download for Https {
    fn fetch(&self, url: &str, path: &Path, limit: u64) -> io::Result<()> {
        self.request(url, path, limit, false)?;
        if fs::metadata(path)?.len() > limit {
            return Err(io::Error::other("release download exceeds size limit"));
        }
        Ok(())
    }

    fn resolve(&self, url: &str) -> io::Result<String> {
        self.request(url, Path::new("/dev/null"), MAX_METADATA, true)
    }
}

impl Https {
    fn request(&self, url: &str, path: &Path, limit: u64, head: bool) -> io::Result<String> {
        let mut command = Command::new("curl");
        command
            .args([
                // Local curl defaults must not weaken verification or rewrite output.
                "--disable",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--tlsv1.2",
                "-fsSL",
                "--connect-timeout",
                "10",
                "--max-time",
                "120",
                "--retry",
                "2",
                "--retry-max-time",
                "30",
                "--retry-connrefused",
                "--max-redirs",
                "5",
                "--max-filesize",
                &limit.to_string(),
                "--user-agent",
                "uniterm-updater",
                "--write-out",
                "%{http_code}\n%{url_effective}",
                "--output",
            ])
            .arg(path);
        if head {
            command.arg("--head");
        }
        let output = command
            .arg(url)
            .output()
            .map_err(|e| io::Error::other(format!("curl is required for updates: {e}")))?;
        let transfer = String::from_utf8_lossy(&output.stdout);
        let (status, effective_url) = transfer.trim().split_once('\n').unwrap_or(("000", ""));
        if !output.status.success() {
            let hint = if matches!(status, "403" | "429") {
                "; GitHub or a network proxy refused this request (access restrictions or rate limiting)"
            } else {
                ""
            };
            return Err(io::Error::other(format!(
                "download failed for {url} (HTTP {status}): {}{hint}",
                String::from_utf8_lossy(&output.stderr).trim(),
            )));
        }
        Ok(effective_url.into())
    }
}

fn release_tag(
    options: &Options,
    metadata: &Path,
    downloader: &impl Download,
) -> io::Result<String> {
    let api = match &options.version {
        Some(tag) => format!("https://api.github.com/repos/{REPO}/releases/tags/{tag}"),
        None => format!("https://api.github.com/repos/{REPO}/releases/latest"),
    };
    match downloader.fetch(&api, metadata, MAX_METADATA) {
        Ok(()) => {
            let release: serde_json::Value = serde_json::from_reader(File::open(metadata)?)?;
            if release["draft"] == true || release["prerelease"] == true {
                return Err(io::Error::other("release response is not a stable release"));
            }
            release["tag_name"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| io::Error::other("release response has no version"))
        }
        Err(api_error) => {
            // GitHub's public latest-release redirect has no REST API quota.
            // Resolve it once and pin every asset to that tag, even if another
            // release is published while this update is in progress.
            let latest = format!("https://github.com/{REPO}/releases/latest");
            let resolved = downloader.resolve(&latest).map_err(|error| {
                io::Error::other(format!(
                    "{api_error}; public release fallback failed: {error}"
                ))
            })?;
            let prefix = format!("https://github.com/{REPO}/releases/tag/");
            let tag = resolved.strip_prefix(&prefix).ok_or_else(|| {
                io::Error::other("public release redirect did not identify a Uniterm release")
            })?;
            if options
                .version
                .as_ref()
                .is_some_and(|requested| requested != tag)
            {
                return Err(io::Error::other(format!(
                    "{api_error}; the public fallback identifies {tag}, so it cannot verify the requested older release"
                )));
            }
            Ok(tag.into())
        }
    }
}

struct Stage(PathBuf);
impl Stage {
    fn create(parent: &Path) -> io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self::create_with_nonce(parent, nonce)
    }

    fn create_with_nonce(parent: &Path, nonce: u128) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        // Clock resolution can be coarser than concurrent update checks. The
        // atomic directory creation owns a candidate only on success; never
        // reuse, remove, or follow another operation's existing directory.
        for attempt in 0..128 {
            let path = parent.join(format!(
                ".uniterm-update-{}-{nonce}-{attempt}",
                std::process::id()
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique update staging directory",
        ))
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn checksum(path: &Path, manifest: &str, name: &str) -> io::Result<()> {
    let entries: Vec<_> = manifest
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let hash = words.next()?;
            let file = words.next()?.trim_start_matches('*');
            (file == name && words.next().is_none()).then_some(hash)
        })
        .collect();
    if entries.len() != 1
        || entries[0].len() != 64
        || !entries[0].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(io::Error::other(format!(
            "SHA256SUMS needs exactly one valid entry for {name}"
        )));
    }
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65_536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    if format!("{:x}", hash.finalize()) != entries[0].to_ascii_lowercase() {
        return Err(io::Error::other(format!(
            "checksum mismatch for {name}; installed binaries are unchanged"
        )));
    }
    Ok(())
}

fn install_pair(stage: &mut Stage, directory: &Path) -> io::Result<()> {
    let mut old = Vec::new();
    for name in ["uniterm", "ut"] {
        let path = directory.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(io::Error::other(format!(
                    "refusing to replace directory {}",
                    path.display()
                )))
            }
            Ok(_) => {
                fs::hard_link(&path, stage.0.join(format!("old-{name}")))?;
                old.push(name);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    fs::rename(stage.0.join("uniterm"), directory.join("uniterm"))?;
    if let Err(error) = fs::rename(stage.0.join("ut"), directory.join("ut")) {
        let rollback = if old.contains(&"uniterm") {
            fs::rename(stage.0.join("old-uniterm"), directory.join("uniterm"))
        } else {
            fs::remove_file(directory.join("uniterm"))
        };
        if let Err(rollback) = rollback {
            let retained = std::mem::take(&mut stage.0);
            return Err(io::Error::other(format!("install failed: {error}; rollback failed: {rollback}; recovery files retained in {}",retained.display())));
        }
        return Err(error);
    }
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn execute(
    options: &Options,
    executable: &Path,
    downloader: &impl Download,
) -> io::Result<Outcome> {
    let platform = platform()?;
    let directory = executable
        .parent()
        .ok_or_else(|| io::Error::other("could not locate installation directory"))?;
    // Checks never need write access to the install directory.
    let metadata_stage = Stage::create(&std::env::temp_dir())?;
    let metadata = metadata_stage.0.join("release.json");
    let tag = release_tag(options, &metadata, downloader)?;
    let latest = version(&tag).ok_or_else(|| io::Error::other("release has an invalid version"))?;
    if tag != format!("v{}.{}.{}", latest.0, latest.1, latest.2)
        || options.version.as_ref().is_some_and(|v| v != &tag)
    {
        return Err(io::Error::other(
            "release response does not identify the requested stable version",
        ));
    }
    let current = version(env!("CARGO_PKG_VERSION")).unwrap();
    let available = if options.version.is_some() {
        current != latest || !env!("UNITERM_VERSION_SUFFIX").is_empty()
    } else {
        current < latest || (current == latest && !env!("UNITERM_VERSION_SUFFIX").is_empty())
    };
    let mut result = Outcome {
        current: format!(
            "{}{}",
            env!("CARGO_PKG_VERSION"),
            env!("UNITERM_VERSION_SUFFIX")
        ),
        latest: tag.clone(),
        update_available: available,
        installed: false,
        install_dir: directory.into(),
        message: if available {
            format!("Uniterm {tag} is available")
        } else {
            "Uniterm is up to date".into()
        },
    };
    if options.check || !available {
        return Ok(result);
    }
    let _lock = OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(directory.join(".uniterm-update.lock"))
        .map_err(|e| io::Error::other(format!("cannot update {}: {e}; run this command with an account that can write to the installation directory",directory.display())))?;
    // SAFETY: the open file owns this descriptor for the entire installation.
    if unsafe { libc::flock(_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other(
            "another updater owns the installation lock",
        ));
    }
    let mut stage = Stage::create(directory)?;
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let manifest_path = stage.0.join("SHA256SUMS");
    downloader.fetch(&format!("{base}/SHA256SUMS"), &manifest_path, MAX_METADATA)?;
    let manifest = fs::read_to_string(manifest_path)?;
    for name in ["uniterm", "ut"] {
        let asset = format!("{name}-{platform}");
        let path = stage.0.join(name);
        downloader.fetch(&format!("{base}/{asset}"), &path, MAX_BINARY)?;
        checksum(&path, &manifest, &asset)?;
    }
    // Execute only verified release bytes, before replacing either binary.
    for name in ["uniterm", "ut"] {
        let path = stage.0.join(name);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        let output = Command::new(&path).arg("--version").output()?;
        if !output.status.success()
            || String::from_utf8_lossy(&output.stdout).trim() != format!("uniterm {}", &tag[1..])
        {
            return Err(io::Error::other(format!(
                "{name} release binary failed validation; installed binaries are unchanged"
            )));
        }
    }
    install_pair(&mut stage, directory)?;
    result.installed = true;
    result.update_available = false;
    result.message=format!("Installed Uniterm {tag}. Running Workspaces keep their current server until restarted; this update did not stop them.");
    Ok(result)
}

pub(super) fn command(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("Usage: ut update [--check] [--version VERSION] [--json]\nCheck or install a verified public release in the current binary's directory.\nRunning Workspaces are left intact; no automatic checks run in the background.");
        return 0;
    }
    let options = match options(args) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("uniterm update: {}", super::terminal_safe(&error));
            return 2;
        }
    };
    let result = std::env::current_exe().and_then(|exe| execute(&options, &exe, &Https));
    match result {
        Ok(result) => {
            if options.json {
                println!("{}", serde_json::to_string(&result).unwrap());
            } else {
                println!("{}", result.message);
            }
            0
        }
        Err(error) => {
            eprintln!(
                "uniterm update: {}",
                super::terminal_safe(&error.to_string())
            );
            1
        }
    }
}

/// Menu handoff: restore cooked input, check once, and ask before replacing
/// the local binaries. The caller reattaches its existing Workspace afterward.
pub(super) fn interactive() {
    use std::io::Write as _;
    let run = || -> io::Result<()> {
        let executable = std::env::current_exe()?;
        let check = execute(
            &Options {
                check: true,
                ..Options::default()
            },
            &executable,
            &Https,
        )?;
        println!(
            "{}\nLocal installation: {}",
            check.message,
            super::terminal_safe(&check.install_dir.display().to_string())
        );
        if check.update_available {
            print!("Install {}? [y/N] ", check.latest);
            io::stdout().flush()?;
            let mut reply = String::new();
            io::stdin().read_line(&mut reply)?;
            if matches!(reply.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                let installed = execute(
                    &Options {
                        version: Some(check.latest),
                        ..Options::default()
                    },
                    &executable,
                    &Https,
                )?;
                println!("{}", installed.message);
            } else {
                println!("Update cancelled.");
            }
        }
        Ok(())
    };
    if let Err(error) = run() {
        eprintln!(
            "uniterm update: {}",
            super::terminal_safe(&error.to_string())
        );
    }
    print!("Press Enter to return to your Workspace.");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().read_line(&mut line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn concurrent_update_stages_with_the_same_clock_value_remain_private_and_distinct() {
        let root = Stage::create(&std::env::temp_dir()).unwrap();
        let existing = Stage::create_with_nonce(&root.0, 42).unwrap();
        fs::write(existing.0.join("preserve"), b"existing operation").unwrap();
        let barrier = std::sync::Barrier::new(16);
        let stages = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        Stage::create_with_nonce(&root.0, 42).unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        let paths: std::collections::HashSet<_> = stages.iter().map(|stage| &stage.0).collect();
        assert_eq!(paths.len(), 16);
        assert!(!paths.contains(&existing.0));
        assert_eq!(
            fs::read(existing.0.join("preserve")).unwrap(),
            b"existing operation"
        );
        for stage in &stages {
            assert_eq!(
                fs::metadata(&stage.0).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        drop(stages);
        assert!(existing.0.join("preserve").exists());
    }

    struct Fixture {
        corrupt: bool,
        api_blocked: bool,
        redirect: String,
        requests: RefCell<Vec<String>>,
    }
    const BINARY: &[u8] = b"#!/bin/sh\nprintf 'uniterm 9.9.9\\n'\n";
    impl Download for Fixture {
        fn fetch(&self, url: &str, path: &Path, _limit: u64) -> io::Result<()> {
            self.requests.borrow_mut().push(url.into());
            if url.starts_with("https://api.github.com/") {
                if self.api_blocked {
                    return Err(io::Error::other("HTTP 403: API rate limit exceeded"));
                }
                fs::write(
                    path,
                    br#"{"tag_name":"v9.9.9","draft":false,"prerelease":false}"#,
                )
            } else if url.ends_with("/SHA256SUMS") {
                let hash = format!("{:x}", Sha256::digest(BINARY));
                fs::write(
                    path,
                    format!(
                        "{hash}  uniterm-{}\n{hash}  ut-{}\n",
                        platform()?,
                        platform()?
                    ),
                )
            } else if self.corrupt && url.contains("/ut-") {
                fs::write(path, b"bad")
            } else {
                fs::write(path, BINARY)
            }
        }

        fn resolve(&self, url: &str) -> io::Result<String> {
            self.requests.borrow_mut().push(url.into());
            Ok(self.redirect.clone())
        }
    }
    fn fixture(corrupt: bool) -> Fixture {
        Fixture {
            corrupt,
            api_blocked: false,
            redirect: format!("https://github.com/{REPO}/releases/tag/v9.9.9"),
            requests: RefCell::new(Vec::new()),
        }
    }
    fn installed() -> Stage {
        let stage = Stage::create(&std::env::temp_dir()).unwrap();
        fs::write(stage.0.join("uniterm"), b"original long name").unwrap();
        fs::write(stage.0.join("ut"), b"original alias").unwrap();
        stage
    }
    #[test]
    fn check_is_read_only_and_requests_only_release_metadata() {
        let dir = installed();
        let download = fixture(false);
        let result = execute(
            &Options {
                check: true,
                ..Options::default()
            },
            &dir.0.join("ut"),
            &download,
        )
        .unwrap();
        assert!(result.update_available);
        assert!(!result.installed);
        assert_eq!(download.requests.borrow().len(), 1);
        assert_eq!(fs::read(dir.0.join("ut")).unwrap(), b"original alias");
        assert!(!dir.0.join(".uniterm-update.lock").exists());
    }
    #[test]
    fn latest_older_than_current_never_downgrades_or_touches_installation() {
        struct Older;
        impl Download for Older {
            fn fetch(&self, url: &str, path: &Path, _: u64) -> io::Result<()> {
                assert!(url.ends_with("/releases/latest"));
                fs::write(
                    path,
                    br#"{"tag_name":"v0.0.0","draft":false,"prerelease":false}"#,
                )
            }

            fn resolve(&self, _: &str) -> io::Result<String> {
                panic!("successful API requests must not use the fallback")
            }
        }
        let dir = installed();
        let result = execute(&Options::default(), &dir.0.join("ut"), &Older).unwrap();
        assert!(!result.update_available);
        assert!(!result.installed);
        assert_eq!(fs::read(dir.0.join("ut")).unwrap(), b"original alias");
        assert!(!dir.0.join(".uniterm-update.lock").exists());
    }
    #[test]
    fn corrupt_second_asset_leaves_both_installed_binaries_unchanged() {
        let dir = installed();
        let error = execute(&Options::default(), &dir.0.join("ut"), &fixture(true))
            .err()
            .unwrap();
        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(fs::read(dir.0.join("ut")).unwrap(), b"original alias");
        assert_eq!(
            fs::read(dir.0.join("uniterm")).unwrap(),
            b"original long name"
        );
    }
    #[test]
    fn verified_update_replaces_both_names_and_preserves_an_open_executable() {
        let dir = installed();
        let mut old = File::open(dir.0.join("ut")).unwrap();
        let result = execute(&Options::default(), &dir.0.join("ut"), &fixture(false)).unwrap();
        assert!(result.installed);
        for name in ["uniterm", "ut"] {
            assert_eq!(fs::read(dir.0.join(name)).unwrap(), BINARY);
            assert_eq!(
                fs::metadata(dir.0.join(name)).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        let mut bytes = Vec::new();
        old.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original alias");
    }
    #[test]
    fn api_rate_limit_falls_back_and_pins_all_verified_assets_to_one_release() {
        let dir = installed();
        let mut download = fixture(false);
        download.api_blocked = true;
        let result = execute(&Options::default(), &dir.0.join("ut"), &download).unwrap();
        assert!(result.installed);
        assert_eq!(result.latest, "v9.9.9");
        let requests = download.requests.borrow();
        assert_eq!(requests.len(), 5);
        assert_eq!(
            requests[1],
            format!("https://github.com/{REPO}/releases/latest")
        );
        for url in &requests[2..] {
            assert!(url.starts_with(&format!(
                "https://github.com/{REPO}/releases/download/v9.9.9/"
            )));
        }
    }
    #[test]
    fn explicitly_pinned_latest_release_can_use_the_public_fallback() {
        let dir = installed();
        let mut download = fixture(false);
        download.api_blocked = true;
        let result = execute(
            &Options {
                version: Some("v9.9.9".into()),
                ..Options::default()
            },
            &dir.0.join("ut"),
            &download,
        )
        .unwrap();
        assert!(result.installed);
        assert!(download.requests.borrow()[0].ends_with("/releases/tags/v9.9.9"));
    }
    #[test]
    fn fallback_checks_and_up_to_date_releases_never_touch_the_installation() {
        for (check, tag) in [(true, "v9.9.9"), (false, "v0.0.0")] {
            let dir = installed();
            let mut download = fixture(false);
            download.api_blocked = true;
            download.redirect = format!("https://github.com/{REPO}/releases/tag/{tag}");
            let result = execute(
                &Options {
                    check,
                    ..Options::default()
                },
                &dir.0.join("ut"),
                &download,
            )
            .unwrap();
            assert!(!result.installed);
            assert_eq!(download.requests.borrow().len(), 2);
            assert_eq!(fs::read(dir.0.join("ut")).unwrap(), b"original alias");
            assert!(!dir.0.join(".uniterm-update.lock").exists());
        }
    }
    #[test]
    fn fallback_still_rejects_corrupt_assets_before_replacing_either_binary() {
        let dir = installed();
        let mut download = fixture(true);
        download.api_blocked = true;
        let error = execute(&Options::default(), &dir.0.join("ut"), &download)
            .err()
            .unwrap();
        assert!(error.to_string().contains("checksum mismatch"));
        assert_eq!(fs::read(dir.0.join("ut")).unwrap(), b"original alias");
        assert_eq!(
            fs::read(dir.0.join("uniterm")).unwrap(),
            b"original long name"
        );
    }
    #[test]
    fn fallback_rejects_foreign_redirects_invalid_tags_and_wrong_pinned_versions() {
        for redirect in [
            "https://example.com/releases/tag/v9.9.9".to_owned(),
            format!("http://github.com/{REPO}/releases/tag/v9.9.9"),
            format!("https://github.com/{REPO}/releases/tag/v9.9.9/other"),
            format!("https://github.com/{REPO}/releases/tag/v9.9.9-rc1"),
            format!("https://github.com/{REPO}/releases/tag/v09.9.9"),
            format!("https://github.com/{REPO}/releases/tag/v8.8.8"),
        ] {
            let dir = installed();
            let mut download = fixture(false);
            download.api_blocked = true;
            download.redirect = redirect;
            let result = execute(
                &Options {
                    version: Some("v9.9.9".into()),
                    ..Options::default()
                },
                &dir.0.join("ut"),
                &download,
            );
            assert!(result.is_err());
            assert_eq!(fs::read(dir.0.join("ut")).unwrap(), b"original alias");
            assert!(!dir.0.join(".uniterm-update.lock").exists());
            assert_eq!(download.requests.borrow().len(), 2);
        }
    }
    #[test]
    fn failed_second_rename_rolls_back_the_first_binary() {
        let dir = installed();
        let mut stage = Stage::create(&dir.0).unwrap();
        fs::write(stage.0.join("uniterm"), b"replacement").unwrap();
        // Deliberately missing staged alias models a failed second rename.
        assert!(install_pair(&mut stage, &dir.0).is_err());
        assert_eq!(
            fs::read(dir.0.join("uniterm")).unwrap(),
            b"original long name"
        );
        assert_eq!(fs::read(dir.0.join("ut")).unwrap(), b"original alias");
    }
    #[test]
    fn release_versions_and_manifests_are_strict() {
        for value in ["../../latest", "1.2.3/other", "1.2", "1.2.3-rc1", "1.2.3.4"] {
            assert!(version(value).is_none());
        }
        assert_eq!(version("v1.2.1"), Some((1, 2, 1)));
        let dir = installed();
        let path = dir.0.join("ut");
        let hash = format!("{:x}", Sha256::digest(b"original alias"));
        assert!(checksum(&path, &format!("{hash} ut\n{hash} ut\n"), "ut").is_err());
        assert!(checksum(&path, &format!("{hash} ut\n"), "ut").is_ok());
    }
}
