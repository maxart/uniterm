//! Workspace socket paths, listener binding, the process-lifetime Workspace
//! lock, and the config-file entry points the CLI calls.
//!
//! The socket directory is owner-only and validated on every bind, so a shared
//! `/tmp` fallback can never hand a Workspace to another user.

use super::*;

#[cfg(any(target_os = "android", test))]
use std::ffi::OsString;

/// A process-lifetime claim on one Workspace name.
///
/// Two claims are held for the lifetime of the guard, because the two things
/// a Workspace owns are resolved independently and can disagree:
///
/// - the socket claim, next to the socket, so a live server with an
///   accidentally unlinked socket stays protected (`flock`);
/// - the durable-state claim, inside the state directory, so no other process
///   can ever start a server, or run a maintenance command, against the same
///   event stream and snapshot. `$XDG_RUNTIME_DIR` differs between a desktop
///   login, an SSH session without a PAM session, a systemd user unit, and a
///   test harness, while `$XDG_STATE_HOME` is the same for all of them. A
///   second server that shared only the state directory would, on its clean
///   stop, delete the stream a still-live server keeps appending to.
///
/// The durable claim is a POSIX record lock, which is per process rather than
/// per open file description: the guard is protecting durable files from
/// *other processes*, and one process must be free to re-take the claim of a
/// name it already owns (integration tests bind several Workspaces in one
/// process).
#[derive(Debug)]
pub struct WorkspaceLock {
    _socket_claim: std::fs::File,
    _state_claim: std::fs::File,
}

impl WorkspaceLock {
    /// Claim exclusive ownership of a Workspace until this guard is dropped.
    ///
    /// Maintenance commands use the same claim before changing stopped state,
    /// so a live server with an accidentally unlinked socket remains protected.
    pub fn acquire(socket: &Path) -> std::io::Result<Self> {
        prepare_socket_parent(socket)?;
        let name = socket
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Workspace socket needs a UTF-8 file stem",
                )
            })?;
        let socket_claim = Self::open_claim(&socket.with_extension("lock"))?;
        // SAFETY: `socket_claim` owns a valid descriptor for the lifetime of the lock.
        if unsafe { libc::flock(socket_claim.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
            return Err(Self::busy_error(
                std::io::Error::last_os_error(),
                name,
                "socket",
            ));
        }
        let state_path = crate::persist::lock_path(name);
        if let Some(parent) = state_path.parent() {
            crate::persist::ensure_private_dir(parent)?;
        }
        let state_claim = Self::open_claim(&state_path)?;
        let mut record = libc::flock {
            l_type: libc::F_WRLCK as libc::c_short,
            l_whence: libc::SEEK_SET as libc::c_short,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        // SAFETY: `state_claim` owns a valid descriptor and `record` is a
        // fully initialised, properly aligned `flock` that outlives the call.
        if unsafe { libc::fcntl(state_claim.as_raw_fd(), libc::F_SETLK, &mut record) } == -1 {
            return Err(Self::busy_error(
                std::io::Error::last_os_error(),
                name,
                "durable state",
            ));
        }
        Ok(Self {
            _socket_claim: socket_claim,
            _state_claim: state_claim,
        })
    }

    fn open_claim(path: &Path) -> std::io::Result<std::fs::File> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        // SAFETY: getuid has no preconditions and does not access memory.
        let uid = unsafe { libc::getuid() };
        if !metadata.is_file() || metadata.uid() != uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "Workspace lock {} is not a regular file owned by this user",
                    path.display()
                ),
            ));
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }

    fn busy_error(error: std::io::Error, name: &str, claim: &str) -> std::io::Error {
        // `flock` reports EWOULDBLOCK; `fcntl(F_SETLK)` reports EACCES or
        // EAGAIN, which std maps to PermissionDenied or WouldBlock.
        let busy = matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::PermissionDenied
        ) || matches!(error.raw_os_error(), Some(libc::EACCES | libc::EAGAIN));
        if busy {
            std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!(
                    "Workspace '{name}' is already running (another process holds its {claim} claim; its socket may be unavailable or under a different runtime directory)"
                ),
            )
        } else {
            error
        }
    }
}

pub(super) fn prepare_socket_parent(sock_path: &Path) -> std::io::Result<()> {
    let Some(dir) = sock_path.parent() else {
        return Ok(());
    };
    if dir == socket_dir() {
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            if let Some(fallback_base) = dir.parent() {
                ensure_private_socket_dir(fallback_base)?;
            }
        }
        ensure_private_socket_dir(dir)
    } else {
        std::fs::create_dir_all(dir).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "could not create socket directory {}: {error}",
                    dir.display()
                ),
            )
        })
    }
}

pub(super) fn ensure_private_socket_dir(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => validate_socket_dir(dir, &metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(dir)?;
            validate_socket_dir(dir, &std::fs::symlink_metadata(dir)?)?;
        }
        Err(error) => return Err(error),
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

pub(super) fn validate_socket_dir(dir: &Path, metadata: &std::fs::Metadata) -> std::io::Result<()> {
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("socket directory {} is not a real directory", dir.display()),
        ));
    }
    // SAFETY: getuid has no preconditions and does not access memory.
    let uid = unsafe { libc::getuid() };
    if metadata.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "socket directory {} is not owned by this user",
                dir.display()
            ),
        ));
    }
    Ok(())
}

pub(super) fn socket_identity(path: &Path) -> std::io::Result<(u64, u64)> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("refusing to replace non-socket path {}", path.display()),
        ));
    }
    Ok((metadata.dev(), metadata.ino()))
}

pub(super) fn remove_socket_if_unchanged(
    path: &Path,
    expected: (u64, u64),
) -> std::io::Result<bool> {
    match socket_identity(path) {
        Ok(current) if current == expected => {
            std::fs::remove_file(path)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

pub(super) fn bind_workspace_listener(path: &Path) -> std::io::Result<UnixListener> {
    for _ in 0..3 {
        match UnixListener::bind(path) {
            Ok(listener) => {
                let identity = socket_identity(path)?;
                if let Err(error) =
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                {
                    drop(listener);
                    let _ = remove_socket_if_unchanged(path, identity);
                    return Err(std::io::Error::new(
                        error.kind(),
                        format!(
                            "could not restrict Workspace socket {} to owner access: {error}",
                            path.display()
                        ),
                    ));
                }
                return Ok(listener);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                let identity = socket_identity(path)?;
                match std::os::unix::net::UnixStream::connect(path) {
                    Ok(_) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::AddrInUse,
                            format!("a Workspace is already listening at {}", path.display()),
                        ));
                    }
                    Err(connect_error)
                        if matches!(
                            connect_error.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) => {}
                    Err(connect_error) => return Err(connect_error),
                }
                if !remove_socket_if_unchanged(path, identity)? {
                    continue;
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!("Workspace socket {} changed while binding", path.display()),
    ))
}

#[cfg(any(target_os = "android", test))]
pub(super) fn android_temp_base(
    tmpdir: Option<OsString>,
    prefix: Option<OsString>,
    platform_default: PathBuf,
) -> PathBuf {
    tmpdir
        .map(PathBuf::from)
        .or_else(|| prefix.map(|value| PathBuf::from(value).join("tmp")))
        .unwrap_or(platform_default)
}

pub(super) fn fallback_socket_base(uid: libc::uid_t) -> PathBuf {
    #[cfg(target_os = "android")]
    let temp = android_temp_base(
        std::env::var_os("TMPDIR"),
        std::env::var_os("PREFIX"),
        std::env::temp_dir(),
    );
    #[cfg(not(target_os = "android"))]
    let temp = PathBuf::from("/tmp");

    temp.join(format!("uniterm-{uid}"))
}

/// The directory holding this user's server sockets.
///
/// Uniterm uses `$XDG_RUNTIME_DIR/uniterm/` when configured.
/// Android falls back to Termux's `$TMPDIR` or `$PREFIX/tmp`; other supported
/// operating systems retain the existing `/tmp/uniterm-<uid>` fallback.
pub fn socket_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // SAFETY: getuid is always safe.
            let uid = unsafe { libc::getuid() };
            fallback_socket_base(uid)
        });
    base.join("uniterm")
}

/// The default socket path for a named server: `<socket_dir>/<name>.sock`.
pub fn default_socket_path(name: &str) -> PathBuf {
    socket_dir().join(format!("{name}.sock"))
}

/// The config file path: `$XDG_CONFIG_HOME/uniterm/uniterm.conf`, falling back
/// to `~/.config/uniterm/uniterm.conf`.
pub fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("uniterm").join("uniterm.conf"));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("uniterm")
            .join("uniterm.conf"),
    )
}

/// Load the config from disk, or defaults if the file is absent/unreadable.
pub fn load_config() -> Config {
    config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|t| Config::parse(&t))
        .unwrap_or_default()
}

/// Convenience entry point used by the CLI: bind, apply config, run to completion.
pub fn run_server(sock_path: &Path, program: &str, args: &[&str]) -> std::io::Result<()> {
    let workspace = sock_path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Workspace socket needs a UTF-8 file stem",
            )
        })?;
    uniterm_proto::validate_workspace_name(workspace).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid Workspace name '{workspace}': {error}"),
        )
    })?;
    let (mut server, mut poll) = Server::bind_internal(sock_path, program, args, 80, 24, false)?;
    server.workspace_catalog_enabled = true;
    let config = load_config();
    let restore = config.restore;
    server.set_config(config);
    server.recover_workspace(poll.registry(), restore)?;
    server.run(&mut poll)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_temp_base_prefers_termux_environment() {
        let prefix = OsString::from("/data/data/com.termux/files/usr");
        let tmpdir = OsString::from("/data/data/com.termux/files/usr/tmp-custom");
        let platform_default = PathBuf::from("/data/local/tmp");

        assert_eq!(
            android_temp_base(
                Some(tmpdir.clone()),
                Some(prefix.clone()),
                platform_default.clone(),
            ),
            PathBuf::from(tmpdir)
        );
        assert_eq!(
            android_temp_base(None, Some(prefix.clone()), platform_default.clone()),
            PathBuf::from(prefix).join("tmp")
        );
        assert_eq!(
            android_temp_base(None, None, platform_default.clone()),
            platform_default
        );
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn non_android_socket_fallback_remains_in_global_tmp() {
        assert_eq!(
            fallback_socket_base(1234),
            PathBuf::from("/tmp/uniterm-1234")
        );
    }

    fn socket_test_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "uniterm-socket-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn socket_paths_are_owner_only_and_live_listeners_are_not_replaced() {
        let dir = socket_test_dir("permissions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_socket_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let path = dir.join("secure.sock");
        let listener = bind_workspace_listener(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let error = bind_workspace_listener(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());

        drop(listener);
        let replacement = bind_workspace_listener(&path).unwrap();
        drop(replacement);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn socket_binding_never_removes_a_non_socket_file() {
        let dir = socket_test_dir("regular-file");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("keep.sock");
        std::fs::write(&path, b"user data").unwrap();
        assert!(bind_workspace_listener(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"user data");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn workspace_lock_outlives_the_socket_path() {
        let dir = socket_test_dir("lifetime-lock");
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_STATE_HOME", dir.join("state"));
        let path = dir.join("locked.sock");
        let first = WorkspaceLock::acquire(&path).unwrap();
        // The durable claim lives with the durable files, not the socket.
        assert!(crate::persist::lock_path("locked").starts_with(dir.join("state")));
        assert!(crate::persist::lock_path("locked").is_file());
        std::fs::write(&path, b"pathname placeholder").unwrap();
        std::fs::remove_file(&path).unwrap();

        let error = WorkspaceLock::acquire(&path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);

        drop(first);
        let replacement = WorkspaceLock::acquire(&path).unwrap();
        drop(replacement);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
