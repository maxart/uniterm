//! Explicit server activation. The installed CLI selects a Workspace, while
//! the serving engine independently selects and verifies the release image.

use super::{
    install_lock, options, platform, release_tag, trusted_directory, verify_installed_pair,
    version, workspace_build, Download, Https, Options, Stage, MAX_METADATA, REPO,
};
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct ServingImage {
    directory: PathBuf,
    identity: Option<(u64, u64)>,
}

static SERVER_INSTALLATION: std::sync::OnceLock<ServingImage> = std::sync::OnceLock::new();

pub(crate) fn enable_server_activation() {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let image = std::path::Path::new("/proc/self/exe");
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            let image = exe.as_path();
            let identity = fs::metadata(image).ok().map(|m| (m.dev(), m.ino()));
            let _ = SERVER_INSTALLATION.set(ServingImage {
                directory: dir.to_owned(),
                identity,
            });
            uniterm_server::server::handoff::register_launcher(launch_verified);
        }
    }
}

fn launch_verified(
    workspace: &str,
    release: &str,
    channel: std::os::unix::net::UnixStream,
) -> io::Result<Option<std::process::Child>> {
    use std::os::fd::OwnedFd;
    let (a, b, c) = version(release).ok_or_else(|| io::Error::other("invalid release number"))?;
    if release != format!("v{a}.{b}.{c}") {
        return Err(io::Error::other("expected a canonical release tag"));
    }
    let serving = SERVER_INSTALLATION
        .get()
        .ok_or_else(|| io::Error::other("unknown installation directory"))?;
    let directory = trusted_directory(&serving.directory, false)?;
    let stage = Stage::create(&std::env::temp_dir())?;
    let metadata = stage.0.join("release.json");
    let requested = Options {
        version: Some(release.to_owned()),
        ..Default::default()
    };
    let tag = release_tag(&requested, &metadata, &Https)?;
    if tag != release {
        return Err(io::Error::other("release metadata changed"));
    }
    let sums = stage.0.join("SHA256SUMS");
    Https.fetch(
        &format!("https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS"),
        &sums,
        MAX_METADATA,
    )?;
    let manifest = fs::read_to_string(sums)?;
    let _pin = install_lock(&directory, libc::LOCK_SH)?;
    verify_installed_pair(&directory, &manifest, platform()?)?;
    if release.strip_prefix('v') == Some(uniterm_proto::BUILD_VERSION) {
        for name in ["uniterm", "ut"] {
            let current = fs::metadata(directory.join(name))?;
            if serving.identity == Some((current.dev(), current.ino())) {
                return Ok(None);
            }
        }
    }
    Command::new(directory.join("uniterm"))
        .args(["--internal-live-server", workspace])
        .stdin(Stdio::from(OwnedFd::from(channel)))
        .spawn()
        .map(Some)
}

pub(crate) fn candidate(args: &[String]) -> i32 {
    use std::os::fd::AsFd;
    let result = (|| -> io::Result<()> {
        let [name] = args else {
            return Err(io::Error::other("expected Workspace name"));
        };
        let path = crate::checked_socket_path(name).map_err(io::Error::other)?;
        let channel =
            std::os::unix::net::UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
        channel.peer_addr()?;
        // Drop the extra socket endpoint before any subprocess can inherit stdin.
        let null = File::open("/dev/null")?;
        // SAFETY: the duplicated channel remains owned, fd zero is reserved for stdin.
        if unsafe { libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO) } < 0 {
            return Err(io::Error::last_os_error());
        }
        enable_server_activation();
        uniterm_server::server::handoff::run_candidate(
            &path,
            name,
            channel,
            &format!("v{}", uniterm_proto::BUILD_VERSION),
        )
    })();
    if let Err(error) = result {
        eprintln!(
            "uniterm: live Workspace bootstrap: {}",
            crate::terminal_safe(&error.to_string())
        );
        return 1;
    }
    0
}

pub(super) fn activate_server(args: &[String]) -> i32 {
    let parsed = crate::project_socket(args).and_then(|(socket, mut rest)| {
        rest.retain(|a| a != "--activate-server");
        let options = options(&rest)?;
        if options.check {
            return Err("--check cannot be combined with --activate-server".into());
        }
        Ok((socket, options))
    });
    let (socket, options) = match parsed {
        Ok(value) => value,
        Err(error) => {
            eprintln!("uniterm update: {}", crate::terminal_safe(&error));
            return 2;
        }
    };
    let release = options
        .version
        .unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION")));
    let workspace = socket.file_stem().unwrap_or_default().to_string_lossy();
    let format = uniterm_server::server::handoff::CHECKPOINT_VERSION;
    let (activated, version, error) = match workspace_build(&socket) {
        Ok(Some(build)) if build.live_handoff == Some(format) => {
            match uniterm_client::activate_server(&socket, release) {
                Ok(report) => (Some(report.activated), Some(report.version), report.error),
                Err(error) => (
                    None,
                    None,
                    Some(format!("Activation outcome unknown ({error}). Check the serving build with ut update --status.")),
                ),
            }
        }
        Ok(build) => (
            Some(false),
            build.map(|build| build.version),
            Some("running Workspace does not support this live update format".into()),
        ),
        Err(error) => (Some(false), None, Some(error.to_string())),
    };
    if options.json {
        println!(
            "{}",
            serde_json::json!({"workspace":workspace,"activated":activated,"version":version,"error":error})
        );
    } else if let Some(message) = &error {
        eprintln!("uniterm update: {}", crate::terminal_safe(message));
    } else if let Some(version) = &version {
        let verb = if activated == Some(true) {
            "now runs"
        } else {
            "already runs"
        };
        println!(
            "Workspace {} {verb} {}",
            crate::terminal_safe(&workspace),
            crate::terminal_safe(version)
        );
    }
    i32::from(error.is_some())
}
