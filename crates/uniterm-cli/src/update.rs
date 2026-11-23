//! Explicit, checksum-verified replacement of the installed binary pair.
//! Release verification runs only on explicit request, off the Workspace core loop.

mod live_server;
pub(super) use live_server::{candidate, enable_server_activation};

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
    /// Release metadata retained across the gap between install and activation.
    #[serde(skip)]
    verified_manifest: Option<String>,
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
        let mut command = Command::new(trusted_curl()?);
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
            if release["draft"].as_bool() != Some(false)
                || release["prerelease"].as_bool() != Some(false)
            {
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
        Self::create_with_nonce(&trusted_directory(parent, true)?, nonce)
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
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_BINARY {
        return Err(io::Error::other(
            "release asset must be a bounded regular file",
        ));
    }
    let mut file = (&mut file).take(MAX_BINARY + 1);
    let mut hash = Sha256::new();
    let mut bytes = [0; 65_536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    if file.limit() == 0 || format!("{:x}", hash.finalize()) != entries[0].to_ascii_lowercase() {
        return Err(io::Error::other(format!(
            "checksum mismatch for {name}; installed binaries are unchanged"
        )));
    }
    Ok(())
}

fn trusted_owner(metadata: &fs::Metadata) -> bool {
    // SAFETY: geteuid only returns the process's current effective identity.
    metadata.uid() == 0 || metadata.uid() == unsafe { libc::geteuid() }
}

// A checksum protects bytes, but another account must also be unable to
// replace the pathname between verification and exec. A sticky parent may
// contain our private installation or stage, as with /tmp on Unix.
fn trusted_directory(path: &Path, allow_sticky_root: bool) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    let mut private_tree = false;
    for ancestor in canonical.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(ancestor)?;
        let mode = metadata.permissions().mode();
        let sticky = mode & 0o1000 != 0 && (ancestor != canonical || allow_sticky_root);
        let android_system = cfg!(target_os = "android")
            && android_data_parent(ancestor, metadata.uid(), metadata.gid(), mode);
        // Inside an owner-only home, a 0775 directory made with umask 002
        // is still inaccessible to other users through this path.
        let shared_write = mode & 0o002 != 0 || (!private_tree && mode & 0o020 != 0);
        if !metadata.is_dir()
            || (!android_system && (!trusted_owner(&metadata) || shared_write && !sticky))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "update directory is writable or replaceable by another account: {}",
                    ancestor.display()
                ),
            ));
        }
        private_tree |= metadata.uid() == unsafe { libc::geteuid() } && mode & 0o011 == 0;
    }
    Ok(canonical)
}

// Android reserves these package-data parents for the system service account
// (AID_SYSTEM). Never extend this trust to a package or a similarly named path.
fn android_data_parent(path: &Path, uid: u32, gid: u32, mode: u32) -> bool {
    if uid != 1000 || gid != 1000 || mode & 0o002 != 0 {
        return false;
    }
    matches!(
        path.to_str(),
        Some("/data" | "/data/data" | "/data/user" | "/data/user_de")
    ) || path
        .parent()
        .is_some_and(|p| p == Path::new("/data/user") || p == Path::new("/data/user_de"))
        && path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

fn trusted_curl() -> io::Result<PathBuf> {
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if !dir.is_absolute() {
            continue;
        }
        let Ok(binary) = fs::canonicalize(dir.join("curl")) else {
            continue;
        };
        let Some(parent) = binary.parent() else {
            continue;
        };
        if trusted_directory(parent, false).is_err() {
            continue;
        }
        let metadata = fs::symlink_metadata(&binary)?;
        if metadata.is_file()
            && trusted_owner(&metadata)
            && metadata.mode() & 0o022 == 0
            && metadata.mode() & 0o111 != 0
        {
            return Ok(binary);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "updates require curl on a trusted absolute PATH",
    ))
}

fn verify_installed_pair(directory: &Path, manifest: &str, platform: &str) -> io::Result<()> {
    for name in ["uniterm", "ut"] {
        let path = directory.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file()
            || !trusted_owner(&metadata)
            || metadata.mode() & 0o022 != 0
            || metadata.mode() & 0o555 != 0o555
        {
            return Err(io::Error::other(format!(
                "{name} must be an owner-controlled regular executable"
            )));
        }
        checksum(&path, manifest, &format!("{name}-{platform}"))?;
    }
    Ok(())
}

impl Outcome {
    fn pin_verified_installation(&self) -> io::Result<InstallLock> {
        let manifest = self
            .verified_manifest
            .as_deref()
            .ok_or_else(|| io::Error::other("release verification required before activation"))?;
        let lock = install_lock(&self.install_dir, libc::LOCK_SH)?;
        verify_installed_pair(&self.install_dir, manifest, platform()?)?;
        Ok(lock)
    }
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

struct InstallLock(File);

impl Drop for InstallLock {
    fn drop(&mut self) {
        // An unrelated fork can briefly inherit a CLOEXEC descriptor. Releasing
        // the open-file-description lock explicitly makes the next update ready
        // as soon as this transaction finishes.
        // SAFETY: self owns this descriptor throughout the call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

// Both installers and client activation use this lock. A successful exec
// closes the descriptor only after the kernel has opened the selected image.
fn install_lock(directory: &Path, operation: libc::c_int) -> io::Result<InstallLock> {
    let directory = trusted_directory(directory, false)?;
    // The directory inode survives both binary renames. A shared reader can
    // pin a system installation without creating a file or needing sudo.
    let lock = File::open(directory)?;
    loop {
        // SAFETY: lock owns its descriptor until the caller drops it or execs.
        if unsafe { libc::flock(lock.as_raw_fd(), operation | libc::LOCK_NB) } == 0 {
            return Ok(InstallLock(lock));
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => {
                return Err(io::Error::new(
                    error.kind(),
                    "another updater owns the installation lock",
                ))
            }
            _ => return Err(error),
        }
    }
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
    let resolved_directory = fs::canonicalize(directory)?;
    let directory = resolved_directory.as_path();
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
    let development_build = uniterm_proto::BUILD_VERSION != env!("CARGO_PKG_VERSION");
    let available = current < latest
        || (current == latest && development_build)
        || (options.version.is_some() && current != latest);
    let mut result = Outcome {
        current: uniterm_proto::BUILD_VERSION.into(),
        latest: tag.clone(),
        update_available: available,
        installed: false,
        verified_manifest: None,
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
    let _lock = install_lock(directory, libc::LOCK_EX)?;
    let mut stage = Stage::create(directory)?;
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let manifest_path = stage.0.join("SHA256SUMS");
    downloader.fetch(&format!("{base}/SHA256SUMS"), &manifest_path, MAX_METADATA)?;
    let manifest = fs::read_to_string(manifest_path)?;
    // A previous update from another terminal can leave this client running
    // an older image. Reuse only regular executable files that match the
    // verified release manifest. Never probe arbitrary siblings to discover
    // whether another process already installed the release.
    let expected_version = format!("uniterm {}", &tag[1..]);
    if verify_installed_pair(directory, &manifest, platform).is_ok()
        && ["uniterm", "ut"].iter().all(|name| {
            probe(&directory.join(name), &["--version"], &expected_version).unwrap_or(false)
        })
    {
        result.update_available = false;
        result.message = format!("Uniterm {tag} is already installed.");
        result.verified_manifest = Some(manifest);
        return Ok(result);
    }
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
        if !probe(&path, &["--version"], &expected_version)? {
            return Err(io::Error::other(format!(
                "{name} release binary failed validation; installed binaries are unchanged"
            )));
        }
        // Make the verified data and executable mode durable before publishing
        // either directory entry.
        File::open(&path)?.sync_all()?;
    }
    install_pair(&mut stage, directory)?;
    result.verified_manifest = Some(manifest);
    result.installed = true;
    result.update_available = false;
    result.message=format!("Installed Uniterm {tag}. Running Workspaces keep their current server until restarted; this update did not stop them.");
    Ok(result)
}

// Execute the newly installed binary so hooks come from the new release,
// rather than from the still-running updater. Failure does not roll back an
// otherwise successful binary update; the next startup retries from disk.
fn refresh_connectors(result: &mut Outcome) {
    let config = uniterm_server::server::load_config();
    if !result.installed || (!config.auto_update_connectors && !config.auto_install_connectors) {
        return;
    }
    if let Err(error) = result
        .pin_verified_installation()
        .and_then(|_lock| refresh_with_binary(&result.install_dir.join("uniterm")))
    {
        result.message.push_str(&format!(
            " Automatic connector setup failed: {}. It will retry at next startup.",
            super::terminal_safe(&error.to_string())
        ));
    }
}

fn refresh_with_binary(binary: &Path) -> io::Result<()> {
    let output = Command::new(binary)
        .args(["agent", "connector", "auto-update"])
        .stdin(Stdio::null())
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn workspace_build(socket: &Path) -> io::Result<Option<uniterm_proto::WorkspaceBuild>> {
    let reply =
        uniterm_client::control_request(socket, uniterm_proto::ControlCommand::Capabilities)?;
    if let Some(uniterm_proto::ControlResult::Capabilities { server_build, .. }) = reply.result {
        return Ok(server_build);
    }
    Err(io::Error::other(reply.error.map_or_else(
        || "unexpected Workspace response".into(),
        |error| error.message,
    )))
}

fn status(args: &[String]) -> i32 {
    let json = args.iter().any(|arg| arg == "--json");
    let (socket, rest) = match super::project_socket(args) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("uniterm update: {}", super::terminal_safe(&error));
            return 2;
        }
    };
    if rest
        .iter()
        .any(|arg| !matches!(arg.as_str(), "--json" | "--status"))
    {
        eprintln!("usage: ut update --status [--workspace NAME] [--json]");
        return 2;
    }
    let workspace = socket.file_stem().unwrap_or_default().to_string_lossy();
    match workspace_build(&socket) {
        Ok(server) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "workspace": workspace, "cli_version": uniterm_proto::BUILD_VERSION, "server": server
                    })
                );
            } else {
                println!("CLI {}", uniterm_proto::BUILD_VERSION);
                let identity = server.map_or_else(
                    || "version unavailable (older server)".into(),
                    |build| format!("{} (wire protocol {})", build.version, build.wire_protocol),
                );
                println!(
                    "Workspace {} server: {}",
                    super::terminal_safe(&workspace),
                    super::terminal_safe(&identity)
                );
            }
            0
        }
        Err(error) => {
            eprintln!(
                "uniterm update: cannot query Workspace {}: {}",
                super::terminal_safe(&workspace),
                super::terminal_safe(&error.to_string())
            );
            1
        }
    }
}

pub(super) fn command(args: &[String]) -> i32 {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("Usage: ut update [--check] [--version VERSION] [--json]\n       ut update --status [--workspace NAME] [--json]\n       ut update --activate-server [--workspace NAME] [--version VERSION] [--json]\nCheck or install a verified public release in the current binary's directory.\nRunning Workspaces are left intact; no automatic checks run in the background.");
        return 0;
    }
    if args.iter().any(|arg| arg == "--activate-server") {
        return live_server::activate_server(args);
    }
    if args.iter().any(|arg| arg == "--status") {
        return status(args);
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
        Ok(mut result) => {
            refresh_connectors(&mut result);
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

/// The active destination at the moment a menu yields the terminal for an
/// update. SSH proxy sockets expire when their bridge finishes.
pub(super) enum AttachTarget<'a> {
    Local(&'a Path),
    Remote {
        target: &'a str,
        workspace: Option<&'a str>,
        origin: Option<&'a Path>,
    },
}

fn workspace_name(socket: &Path) -> io::Result<&str> {
    socket
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|name| super::checked_socket_path(name).as_deref() == Ok(socket))
        .ok_or_else(|| io::Error::other("could not identify the current Workspace"))
}

fn probe(binary: &Path, args: &[&str], expected: &str) -> io::Result<bool> {
    let output = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == expected)
}

impl AttachTarget<'_> {
    fn command(&self, binary: &Path) -> io::Result<Command> {
        let mut command = Command::new(binary);
        match self {
            Self::Local(socket) => {
                command.args(["attach", workspace_name(socket)?]);
            }
            Self::Remote {
                target,
                workspace,
                origin,
            } => {
                if let Some(socket) = origin {
                    if !probe(
                        binary,
                        &["--resume-remote-client"],
                        "uniterm remote resume 1",
                    )? {
                        return Err(io::Error::other(
                            "the release cannot resume this saved SSH connection",
                        ));
                    }
                    command.args(["--resume-remote-client", workspace_name(socket)?]);
                } else {
                    command.arg("remote");
                }
                command.arg(target);
                command.args(workspace.iter());
            }
        }
        Ok(command)
    }
}

fn resume_installed(outcome: &Outcome, target: &AttachTarget<'_>) -> io::Result<()> {
    let (_lock, mut command) = prepare_client(outcome, target)?;
    Err(command.exec())
}

fn prepare_client(
    outcome: &Outcome,
    target: &AttachTarget<'_>,
) -> io::Result<(InstallLock, Command)> {
    let lock = outcome.pin_verified_installation()?;
    let binary = outcome.install_dir.join("uniterm");
    let version = outcome.latest.trim_start_matches('v');
    if !probe(&binary, &["--version"], &format!("uniterm {version}"))? {
        return Err(io::Error::other(
            "the installed version changed during the update",
        ));
    }
    // The previous client was speaking this protocol on the active connection.
    // An older or newer release may have an incompatible bincode layout.
    let live_protocol = match target {
        AttachTarget::Local(socket) => workspace_build(socket)?.map(|server| server.wire_protocol),
        AttachTarget::Remote { .. } => None,
    };
    let protocol = live_protocol
        .unwrap_or(uniterm_proto::WIRE_PROTOCOL_VERSION)
        .to_string();
    if !probe(
        &binary,
        &["remote-check", "--protocol", &protocol],
        &format!("uniterm remote protocol {protocol}"),
    )? {
        return Err(io::Error::other(
            "the release uses a different Workspace protocol",
        ));
    }
    Ok((lock, target.command(&binary)?))
}

/// Menu handoff: restore cooked input, check once, and ask before replacing
/// the local binaries. The caller reattaches its existing Workspace afterward.
pub(super) fn interactive(target: AttachTarget<'_>) {
    use std::io::Write as _;
    let run = || -> io::Result<Option<Outcome>> {
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
            print!(
                "Install {} and use its compatible client? [y/N] ",
                check.latest
            );
            io::stdout().flush()?;
            let mut reply = String::new();
            io::stdin().read_line(&mut reply)?;
            if matches!(reply.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                let mut installed = execute(
                    &Options {
                        version: Some(check.latest),
                        ..Options::default()
                    },
                    &executable,
                    &Https,
                )?;
                refresh_connectors(&mut installed);
                println!("{}", installed.message);
                return Ok(Some(installed));
            } else {
                println!("Update cancelled.");
            }
        }
        Ok(None)
    };
    match run() {
        Ok(Some(installed)) => {
            if let Err(error) = resume_installed(&installed, &target) {
                eprintln!(
                    "Continuing with the current client: {}",
                    super::terminal_safe(&error.to_string())
                );
            }
        }
        Ok(None) => {}
        Err(error) => eprintln!(
            "uniterm update: {}",
            super::terminal_safe(&error.to_string())
        ),
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
    use std::io::Write;

    // Keep executable writes in a child, like the production curl downloader.
    // Concurrent test spawns can inherit even CLOEXEC write descriptors until
    // they exec, making an otherwise closed fixture temporarily ETXTBSY.
    fn write_executable_fixture(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\"", "fixture-writer"])
            .arg(path)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let written = child.stdin.take().unwrap().write_all(bytes);
        let output = child.wait_with_output()?;
        written?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "fixture writer failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )))
        }
    }

    #[test]
    fn activation_checks_the_release_and_wire_protocol_and_pins_the_installed_pair() {
        let stage = installed();
        let outcome = execute(&Options::default(), &stage.0.join("ut"), &fixture(false)).unwrap();
        let binary = stage.0.join("uniterm");
        let origin = super::super::default_socket_path("origin");
        let target = AttachTarget::Remote {
            target: "work-host",
            workspace: Some("switched"),
            origin: Some(&origin),
        };
        assert!(prepare_client(&outcome, &target)
            .err()
            .unwrap()
            .to_string()
            .contains("different Workspace protocol"));
        let script = format!(
            r#"#!/bin/sh
case "$1" in
    --version) echo 'uniterm 9.9.9' ;;
    remote-check) [ "$2 $3" = '--protocol {wire}' ] && echo 'uniterm remote protocol {wire}' ;;
    --resume-remote-client) if [ "$#" = 1 ]; then echo 'uniterm remote resume 1'; else printf '%s\n' "$@"; fi ;;
    remote|attach) printf '%s\n' "$@" ;;
    *) exit 1 ;;
esac
"#,
            wire = uniterm_proto::WIRE_PROTOCOL_VERSION
        );
        let release = Fixture {
            binary: script.into_bytes(),
            ..fixture(false)
        };
        let outcome = execute(&Options::default(), &stage.0.join("ut"), &release).unwrap();
        let (lock, mut command) = prepare_client(&outcome, &target).unwrap();
        assert!(install_lock(&stage.0, libc::LOCK_EX).is_err());
        assert_eq!(
            command.output().unwrap().stdout,
            b"--resume-remote-client\norigin\nwork-host\nswitched\n"
        );
        drop(lock);
        let local = AttachTarget::Local(&origin)
            .command(&binary)
            .unwrap()
            .output()
            .unwrap();
        assert_eq!(local.stdout, b"attach\norigin\n");
        let direct = AttachTarget::Remote {
            target: "first-host",
            workspace: None,
            origin: None,
        };
        assert_eq!(
            direct.command(&binary).unwrap().output().unwrap().stdout,
            b"remote\nfirst-host\n"
        );
        // A replaced executable can print the expected version. Reject its bytes
        // before permitting even a version probe to execute it.
        let marker = stage.0.join("unverified-ran");
        write_executable_fixture(
            &binary,
            format!(
                "#!/bin/sh\ntouch '{}'\necho 'uniterm 9.9.9'\n",
                marker.display()
            )
            .as_bytes(),
        )
        .unwrap();
        assert!(prepare_client(&outcome, &target)
            .err()
            .unwrap()
            .to_string()
            .contains("checksum mismatch"));
        assert!(!marker.exists());
    }

    #[test]
    fn connector_refresh_executes_the_installed_binary_and_reports_failure() {
        let stage = Stage::create(&std::env::temp_dir()).unwrap();
        let binary = stage.0.join("uniterm");
        write_executable_fixture(
            &binary,
            b"#!/bin/sh\n[ \"$1 $2 $3\" = \"agent connector auto-update\" ] || exit 7\n",
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        refresh_with_binary(&binary).unwrap();
        write_executable_fixture(&binary, b"#!/bin/sh\necho retry-needed >&2\nexit 1\n").unwrap();
        assert!(refresh_with_binary(&binary)
            .unwrap_err()
            .to_string()
            .contains("retry-needed"));
    }

    #[test]
    fn executable_fixtures_remain_executable_during_concurrent_spawns() {
        let root = Stage::create(&std::env::temp_dir()).unwrap();
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let root = &root.0;
                scope.spawn(move || {
                    for iteration in 0..32 {
                        let path = root.join(format!("{worker}-{iteration}"));
                        write_executable_fixture(&path, b"#!/bin/sh\nexit 0\n").unwrap();
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                        assert!(Command::new(&path).status().unwrap().success());
                    }
                });
            }
        });
    }

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
        binary: Vec<u8>,
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
                let hash = format!("{:x}", Sha256::digest(&self.binary));
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
                write_executable_fixture(path, &self.binary)
            }
        }

        fn resolve(&self, url: &str) -> io::Result<String> {
            self.requests.borrow_mut().push(url.into());
            Ok(self.redirect.clone())
        }
    }
    fn fixture(corrupt: bool) -> Fixture {
        Fixture {
            binary: BINARY.to_vec(),
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
    fn activation_rejects_shared_write_access_to_release_images_and_installation() {
        let dir = installed();
        let download = fixture(false);
        let result = execute(&Options::default(), &dir.0.join("ut"), &download).unwrap();
        let alias = dir.0.join("ut");
        fs::set_permissions(&alias, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(result.pin_verified_installation().is_err());
        fs::set_permissions(&alias, fs::Permissions::from_mode(0o755)).unwrap();
        let child = dir.0.join("private");
        fs::create_dir(&child).unwrap();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(trusted_directory(&child, false).is_err());
        assert!(result.pin_verified_installation().is_err());
        assert!(Stage::create(&child).is_err());
        assert!(execute(&Options::default(), &dir.0.join("ut"), &download).is_err());
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(trusted_directory(&child, false).is_ok());
    }

    #[test]
    fn protected_package_parents_and_private_home_subdirectories_are_trusted() {
        for path in ["/data", "/data/data", "/data/user/10", "/data/user_de/0"] {
            assert!(android_data_parent(Path::new(path), 1000, 1000, 0o771));
            assert!(!android_data_parent(Path::new(path), 1000, 1000, 0o777));
            assert!(!android_data_parent(Path::new(path), 1001, 1000, 0o771));
        }
        assert!(!android_data_parent(
            Path::new("/data/user/10/package"),
            1000,
            1000,
            0o771
        ));
        let home = installed();
        let bin = home.0.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o775)).unwrap();
        assert!(trusted_directory(&bin, false).is_ok());
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(trusted_directory(&bin, false).is_err());
    }

    #[test]
    fn activation_can_pin_a_read_only_installation() {
        let stage = installed();
        fs::set_permissions(&stage.0, fs::Permissions::from_mode(0o555)).unwrap();
        let first = install_lock(&stage.0, libc::LOCK_SH).unwrap();
        let second = install_lock(&stage.0, libc::LOCK_SH).unwrap();
        assert_eq!(
            install_lock(&stage.0, libc::LOCK_EX).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop((first, second));
        fs::set_permissions(&stage.0, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(install_lock(&stage.0, libc::LOCK_EX).is_ok());
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
    fn attached_old_image_reuses_only_the_verified_installed_pair() {
        let dir = installed();
        let old_image = dir.0.join("ut");
        let first = execute(&Options::default(), &old_image, &fixture(false)).unwrap();
        assert!(first.installed);
        let retry = fixture(false);
        let second = execute(&Options::default(), &old_image, &retry).unwrap();
        assert!(!second.installed);
        assert!(!second.update_available);
        assert_eq!(retry.requests.borrow().len(), 2);
        assert!(retry.requests.borrow()[1].ends_with("/SHA256SUMS"));
        fs::set_permissions(&old_image, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            execute(&Options::default(), &old_image, &fixture(false))
                .unwrap()
                .installed
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
