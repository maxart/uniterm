//! One local package exposes the entrypoint each OpenCode generation expects.
//! Legacy Bun imports select main/index.ts; v2's server prefers server.ts and
//! its TUI discovers a separate directory. Ordinary installation needs no CLI
//! process; immutable configs retain the previous version-specific fallback.

use std::cell::{Cell, RefCell};
use std::io;
use std::path::{Path, PathBuf};

use jsonc_parser::cst::{CstArray, CstInputValue, CstRootNode};
use jsonc_parser::ParseOptions;

pub(super) const PLUGIN: &str = include_str!("opencode-v1.js");
const SERVER: &str = "// uniterm://cli-agent - lifecycle belongs to the local TUI.\nexport default { id: \"uniterm.notify.v1\", setup() {} }\n";
const MANIFEST: &str =
    "{\n  \"uniterm\": \"uniterm://cli-agent\",\n  \"main\": \"./index.ts\"\n}\n";
const TUI: &str = include_str!("opencode-tui.js");
// Keep the package outside auto-discovery: a configured target and a discovered
// symlink spelling must never activate the same modern plugin twice.
const SPEC: &str = "./uniterm-notify";
const CONFIGS: [&str; 3] = ["config.json", "opencode.json", "opencode.jsonc"];

pub(super) fn plugin_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| Some(super::home()?.join(".config")))?;
    Some(base.join("opencode/uniterm-notify/index.ts"))
}

struct ConfigFile {
    path: PathBuf,
    existed: bool,
    write_path: PathBuf,
    owned_array: Cell<bool>,
    original: String,
    written: RefCell<Option<String>>,
    root: CstRootNode,
}

impl ConfigFile {
    fn read(path: PathBuf) -> io::Result<Self> {
        // Preserve dotfile-manager symlinks, including their logical config base.
        let write_path = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => path.canonicalize()?,
            Ok(_) => path.clone(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => path.clone(),
            Err(error) => return Err(error),
        };
        let (original, existed) = match std::fs::read_to_string(&path) {
            Ok(text) => (text, true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (
                "{\n  \"$schema\": \"https://opencode.ai/config.json\"\n}\n".into(),
                false,
            ),
            Err(error) => return Err(error),
        };
        let root = CstRootNode::parse(
            original.strip_prefix('\u{feff}').unwrap_or(&original),
            &ParseOptions {
                allow_comments: true,
                allow_trailing_commas: true,
                allow_loose_object_property_names: false,
                allow_missing_commas: false,
                allow_single_quoted_strings: false,
                allow_hexadecimal_numbers: false,
                allow_unary_plus_numbers: false,
                allow_bare_decimal_point_numbers: false,
                allow_non_finite_numbers: false,
                allow_extended_string_escapes: false,
            },
        )
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if root.value().is_some() && root.object_value().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OpenCode config must be an object",
            ));
        }
        if let Some(object) = root.object_value() {
            if object
                .properties()
                .iter()
                .filter(|property| property.decoded_name().as_deref() == Some("plugin"))
                .count()
                > 1
                || object.get("plugin").is_some() && object.array_value("plugin").is_none()
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OpenCode plugin must be one unambiguous array; config unchanged",
                ));
            }
        }
        Ok(Self {
            path,
            existed,
            write_path,
            owned_array: Cell::new(false),
            original,
            written: RefCell::new(None),
            root,
        })
    }

    fn plugins(&self) -> Option<CstArray> {
        self.root.object_value()?.array_value("plugin")
    }

    fn managed_count(&self) -> usize {
        self.plugins().map_or(0, |array| {
            array.elements().iter().filter(|node| managed(node)).count()
        })
    }

    fn owns_array(&self) -> bool {
        self.owned_array.get()
    }

    fn rendered(&self) -> String {
        let text = self.root.to_string();
        if self.original.starts_with('\u{feff}') {
            format!("\u{feff}{text}")
        } else {
            text
        }
    }

    fn save(&self) -> io::Result<()> {
        let updated = self.rendered();
        let written = self.written.borrow();
        let expected = written.as_deref().unwrap_or(&self.original);
        if updated != expected {
            let current = std::fs::read_to_string(&self.write_path);
            let unchanged = if self.existed || written.is_some() {
                current.as_ref().is_ok_and(|text| text == expected)
            } else {
                current
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            };
            if !unchanged {
                return Err(io::Error::other(format!(
                    "OpenCode config changed during connector update: {}",
                    self.path.display()
                )));
            }
            write_config(&self.write_path, &updated)?;
            drop(written);
            *self.written.borrow_mut() = Some(updated);
        }
        Ok(())
    }

    fn rollback(&self) -> io::Result<()> {
        let written = self.written.borrow();
        if written.as_ref().is_some_and(|updated| {
            std::fs::read_to_string(&self.write_path).is_ok_and(|text| &text == updated)
        }) {
            if self.existed {
                write_config(&self.write_path, &self.original)?;
            } else {
                remove_file(&self.write_path)?;
            }
        }
        Ok(())
    }
}

fn write_config(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("missing config directory"))?;
    std::fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(
        ".uniterm-config-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let permissions = std::fs::metadata(path).map(|metadata| metadata.permissions());
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| {
        if let Ok(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(text.as_bytes())?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn managed(node: &jsonc_parser::cst::CstNode) -> bool {
    let spec = node
        .as_array()
        .and_then(|array| array.elements().first().cloned())
        .unwrap_or_else(|| node.clone());
    spec.as_string_lit()
        .and_then(|value| value.decoded_value().ok())
        .is_some_and(|value| value == SPEC)
}

struct ConfigFiles {
    files: Vec<ConfigFile>,
    ownership: ConfigFile,
    target_write_denied: Cell<bool>,
}

impl std::ops::Deref for ConfigFiles {
    type Target = [ConfigFile];

    fn deref(&self) -> &Self::Target {
        &self.files
    }
}

impl ConfigFiles {
    fn update_ownership(&self, pending: bool) {
        let names = self
            .files
            .iter()
            .zip(CONFIGS)
            .filter(|(file, _)| file.owns_array() && (pending || file.managed_count() != 0))
            .map(|(_, name)| CstInputValue::String(name.into()))
            .collect::<Vec<_>>();
        if names.is_empty() && !self.ownership.existed {
            return;
        }
        self.ownership.root.set_value(CstInputValue::Object(vec![
            (
                "uniterm".into(),
                CstInputValue::String(super::MARKER.into()),
            ),
            ("arrays".into(), CstInputValue::Array(names)),
            (
                "phase".into(),
                CstInputValue::String(if pending { "pending" } else { "committed" }.into()),
            ),
        ]));
    }
    fn confirm_ownership(&self) -> io::Result<()> {
        self.update_ownership(false);
        self.ownership.save()
    }

    fn ownership_pending(&self) -> bool {
        self.ownership
            .root
            .object_value()
            .and_then(|object| object.get("phase"))
            .and_then(|property| property.value())
            .and_then(|value| value.as_string_lit())
            .and_then(|value| value.decoded_value().ok())
            .is_some_and(|phase| phase == "pending")
    }
}

fn configs(path: &Path) -> io::Result<ConfigFiles> {
    let base = path.parent().and_then(Path::parent).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "OpenCode plugin has no config directory",
        )
    })?;
    let files = CONFIGS
        .iter()
        .map(|name| ConfigFile::read(base.join(name)))
        .collect::<io::Result<Vec<_>>>()?;
    let ownership = ConfigFile::read(path.with_file_name("registration.json"))?;
    if ownership.existed {
        let value: serde_json::Value = serde_json::from_str(&ownership.original)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let names = value
            .get("arrays")
            .and_then(serde_json::Value::as_array)
            .filter(|names| {
                names
                    .iter()
                    .all(|name| name.as_str().is_some_and(|name| CONFIGS.contains(&name)))
            })
            .filter(|_| {
                value.get("uniterm").and_then(serde_json::Value::as_str) == Some(super::MARKER)
                    && matches!(
                        value.get("phase").and_then(serde_json::Value::as_str),
                        Some("pending" | "committed")
                    )
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unrecognized OpenCode connector registration record",
                )
            })?;
        for (file, name) in files.iter().zip(CONFIGS) {
            // If the user removed our entry, the array is no longer ours even
            // if a stale record remains. Never adopt a pre-existing empty array.
            file.owned_array.set(
                file.managed_count() != 0 && names.iter().any(|value| value.as_str() == Some(name)),
            );
        }
    }
    Ok(ConfigFiles {
        files,
        ownership,
        target_write_denied: Cell::new(false),
    })
}

fn selected(files: &[ConfigFile]) -> usize {
    // V1 merges the global files in this order, replacing their plugin arrays.
    // Append to the effective array so no lower-priority user entries disappear.
    files
        .iter()
        .rposition(|file| file.plugins().is_some())
        .or_else(|| files.iter().position(|file| file.existed))
        .unwrap_or(1)
}

fn registered(files: &[ConfigFile]) -> bool {
    files[selected(files)].managed_count() == 1
        && files.iter().map(ConfigFile::managed_count).sum::<usize>() == 1
}

fn register(files: &ConfigFiles, install: bool) -> io::Result<()> {
    if install && registered(files) {
        // A crash after config publication is reconciled from the actual entry.
        return files.confirm_ownership();
    }
    let target = selected(files);
    for file in files.iter() {
        if let Some(array) = file.plugins() {
            for node in array.elements().into_iter().filter(managed) {
                node.remove();
            }
            if array.elements().is_empty() && file.owns_array() {
                file.root
                    .object_value()
                    .unwrap()
                    .get("plugin")
                    .unwrap()
                    .remove();
            }
        }
    }
    if install {
        let object = files[target].root.object_value_or_create().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "OpenCode config must be an object",
            )
        })?;
        files[target]
            .owned_array
            .set(files[target].owns_array() || object.get("plugin").is_none());
        let array = object.array_value_or_create("plugin").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "OpenCode plugin must be an array",
            )
        })?;
        array.append(CstInputValue::String(SPEC.into()));
    }
    files.update_ownership(true);
    let result = (|| {
        // This is an intent record, not a claim that a config write succeeded.
        // Recovery trusts it only when the managed entry is actually present.
        files.ownership.save()?;
        if install {
            files[target].save().inspect_err(|error| {
                files.target_write_denied.set(matches!(
                    error.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem
                ));
            })?;
        }
        for (index, file) in files.iter().enumerate() {
            if !install || index != target {
                file.save()?;
            }
        }
        files.confirm_ownership()?;
        Ok(())
    })();
    rollback_on_error(files, result)
}

fn rollback_on_error(files: &ConfigFiles, result: io::Result<()>) -> io::Result<()> {
    if let Err(error) = result {
        let mut failed = None;
        for file in files.iter().rev() {
            if let Err(rollback) = file.rollback() {
                failed = Some(rollback);
            }
        }
        // Retain intent until authoritative configs are restored. If recovery
        // fails, the next attempt can still reconcile ownership from the files.
        if failed.is_none() {
            failed = files.ownership.rollback().err();
        }
        if let Some(rollback) = failed {
            files.target_write_denied.set(false);
            return Err(io::Error::other(format!(
                "{error}; config rollback failed: {rollback}"
            )));
        }
        return Err(error);
    }
    Ok(())
}

pub(super) fn status() -> super::ConnectorStatus {
    let Some(path) = plugin_path() else {
        return super::ConnectorStatus::NotInstalled;
    };
    status_at(&path)
}

fn status_at(path: &Path) -> super::ConnectorStatus {
    let Some(directory) = path.parent() else {
        return super::ConnectorStatus::NotInstalled;
    };
    let old = old_paths(path);
    let legacy: Vec<_> = old
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    let index = std::fs::read_to_string(directory.join("index.ts")).ok();
    let server = std::fs::read_to_string(directory.join("server.ts")).ok();
    let tui = tui_path(path).and_then(|path| std::fs::read_to_string(path).ok());
    let files = configs(path);
    let registration_valid = files.as_ref().is_ok_and(|files| {
        let absent = old.iter().all(|path| !present(path));
        registered(files) && absent && !files.ownership_pending()
            || files[selected(files)].managed_count() == 0
                && old.iter().skip(1).all(|path| !present(path))
                && match fallback_mode(path, files) {
                    Some(true) => std::fs::read_to_string(&old[0]).is_ok_and(|text| text == PLUGIN),
                    Some(false) => absent,
                    None => false,
                }
    });
    if preflight(path).is_ok()
        && registration_valid
        && !present(&directory.join("tui.ts"))
        && index.as_deref() == Some(PLUGIN)
        && server.as_deref() == Some(SERVER)
        && std::fs::read_to_string(directory.join("package.json"))
            .is_ok_and(|text| text == MANIFEST)
        && tui.as_deref() == Some(TUI)
    {
        return super::ConnectorStatus::Installed;
    }
    if old.iter().any(|path| present(path))
        || [
            directory.join("index.ts"),
            directory.join("server.ts"),
            directory.join("tui.ts"),
        ]
        .iter()
        .any(|path| present(path))
        || tui_path(path).is_some_and(|path| present(&path))
        || legacy
            .iter()
            .chain(index.iter())
            .chain(server.iter())
            .chain(tui.iter())
            .any(|text| text.contains(super::MARKER))
        || files
            .as_ref()
            .is_ok_and(|files| files.iter().any(|file| file.managed_count() != 0))
    {
        super::ConnectorStatus::Outdated
    } else {
        super::ConnectorStatus::NotInstalled
    }
}

// Immutable declarative configs cannot receive a registration. Retain the
// previous version-specific installation in that exceptional case. Its mode is
// recorded only after installation, so status stays spawn-free and retries stop.
// As with the previous installer, switching majors in this mode needs refresh.
fn fallback_record(path: &Path) -> io::Result<Option<serde_json::Value>> {
    let record = path.with_file_name("readonly-fallback");
    let text = match std::fs::read_to_string(record) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if value.get("uniterm").and_then(serde_json::Value::as_str) != Some(super::MARKER)
        || value
            .get("legacy")
            .and_then(serde_json::Value::as_bool)
            .is_none()
        || !value.get("key").is_some_and(serde_json::Value::is_object)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unrecognized OpenCode fallback record",
        ));
    }
    Ok(Some(value))
}

fn file_identity(path: &Path) -> io::Result<serde_json::Value> {
    use std::os::unix::fs::MetadataExt;
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(serde_json::Value::Null)
        }
        Err(error) => return Err(error),
    };
    Ok(serde_json::json!({
        "target": path.canonicalize()?.to_string_lossy(),
        "dev": metadata.dev(), "ino": metadata.ino(), "mode": metadata.mode(),
        "uid": metadata.uid(), "gid": metadata.gid(),
        "size": metadata.size(), "ctime": [metadata.ctime(), metadata.ctime_nsec()],
        "mtime": [metadata.mtime(), metadata.mtime_nsec()],
    }))
}

fn directory_identity(path: &Path) -> io::Result<serde_json::Value> {
    let mut identity = file_identity(path)?;
    if let Some(fields) = identity.as_object_mut() {
        // OpenCode creates unrelated files here. Only directory replacement or
        // access changes should invalidate a successful immutable-config mode.
        for field in ["size", "ctime", "mtime"] {
            fields.remove(field);
        }
    }
    Ok(identity)
}

fn fallback_key(files: &ConfigFiles) -> io::Result<serde_json::Value> {
    let config = &files[selected(files)];
    let executable = crate::workflow::executable_on_search_path(
        "opencode",
        &crate::workflow::search_path_from_env(),
    );
    Ok(serde_json::json!({
        "config": config.path.to_string_lossy(),
        "file": file_identity(&config.write_path)?,
        "parent": directory_identity(config.write_path.parent().ok_or_else(|| io::Error::other("missing config directory"))?)?,
        "executable": executable.as_deref().map(|path| file_identity(Path::new(path))).transpose()?,
    }))
}

fn fallback_mode(path: &Path, files: &ConfigFiles) -> Option<bool> {
    let record = fallback_record(path).ok()??;
    (record.get("key")? == &fallback_key(files).ok()?)
        .then(|| record.get("legacy")?.as_bool())
        .flatten()
}

fn legacy_cli() -> std::io::Result<bool> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = match Command::new("opencode")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    // An explicit version probe has a bounded deadline and a kernel exit
    // watch, never a recurring timer or process-table scan.
    let exited = crate::process_watch::ProcessWatch::new(child.id() as i32)
        .and_then(|watch| watch.wait_exit(std::time::Duration::from_secs(2)))
        .unwrap_or(false);
    if !exited && !child.try_wait().is_ok_and(|status| status.is_some()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "opencode --version timed out; immutable-config fallback unresolved",
        ));
    }
    if !child.wait()?.success() {
        return Err(std::io::Error::other(
            "opencode --version failed; immutable-config fallback unresolved",
        ));
    }
    let mut version = String::new();
    if let Some(stdout) = child.stdout.take() {
        stdout.take(256).read_to_string(&mut version)?;
    }
    legacy_version(&version)
}

fn legacy_version(version: &str) -> std::io::Result<bool> {
    let version = version.trim();
    let version = version.strip_prefix("opencode ").unwrap_or(version);
    match version.trim_start_matches('v').split('.').next() {
        Some("1") => Ok(true),
        Some("2") => Ok(false),
        _ => Err(std::io::Error::other(
            "unrecognized OpenCode version; cannot install read-only-config fallback",
        )),
    }
}

#[test]
fn version_probe_requires_a_known_contract() {
    assert!(legacy_version("1.2.27\n").unwrap());
    assert!(!legacy_version("v2.0.0-beta.3\n").unwrap());
    assert!(!legacy_version("opencode v2.0.26\n").unwrap());
    assert!(legacy_version("error loading configuration").is_err());
    assert!(legacy_version("3.0.0").is_err());
}

fn present(path: &Path) -> bool {
    !matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

// Reject conflicting loader inputs before changing user files. Recognized old
// Uniterm entrypoints can be migrated; manifests and unknown code remain owned
// by the user even when they occupy our conventional directory name.
fn preflight(path: &Path) -> io::Result<()> {
    let outside = path
        .parent()
        .ok_or_else(|| io::Error::other("missing plugin directory"))?;
    let tui = tui_path(path).ok_or_else(|| io::Error::other("missing TUI directory"))?;
    let automatic = tui.parent().unwrap();
    let fallback = path.with_file_name("readonly-fallback");
    if present(&fallback)
        && (fallback_record(path)?.is_none()
            || std::fs::symlink_metadata(&fallback)?
                .file_type()
                .is_symlink())
    {
        return Err(io::Error::other(
            "preserving unrecognized OpenCode fallback record",
        ));
    }
    for directory in [outside, automatic] {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let entrypoint = name
                .split('.')
                .next()
                .is_some_and(|stem| matches!(stem, "index" | "server" | "tui"));
            let owned_manifest = directory == outside
                && name == "package.json"
                && std::fs::read_to_string(entry.path()).is_ok_and(|text| text == MANIFEST)
                && !entry.file_type()?.is_symlink();
            if name == "package.json" && !owned_manifest
                || entrypoint && !matches!(name.as_ref(), "index.ts" | "server.ts" | "tui.ts")
            {
                return Err(io::Error::other(format!(
                    "conflicting OpenCode plugin input: {}",
                    entry.path().display()
                )));
            }
        }
    }
    let mut managed = old_paths(path);
    managed.extend([
        outside.join("index.ts"),
        outside.join("server.ts"),
        outside.join("tui.ts"),
        tui,
    ]);
    for file in managed {
        if !present(&file) {
            continue;
        }
        if std::fs::symlink_metadata(&file)?.file_type().is_symlink()
            || !std::fs::read_to_string(&file)?.contains(super::MARKER)
        {
            return Err(io::Error::other(format!(
                "preserving unrecognized OpenCode plugin file: {}",
                file.display()
            )));
        }
    }
    Ok(())
}

fn remove_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn write_current(path: &Path, text: &str) -> io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|current| current == text) {
        return Ok(());
    }
    super::write_atomic(path, text)
}

fn old_paths(path: &Path) -> Vec<PathBuf> {
    let Some(base) = path.parent().and_then(Path::parent) else {
        return Vec::new();
    };
    let old = base.join("plugins/uniterm-notify");
    vec![
        old.with_extension("ts"),
        old.join("index.ts"),
        old.join("server.ts"),
    ]
}

fn tui_path(path: &Path) -> Option<PathBuf> {
    Some(
        path.parent()?
            .parent()?
            .join("plugins/uniterm-notify/tui.ts"),
    )
}

fn remove_old(path: &Path) -> io::Result<()> {
    let paths = old_paths(path);
    for old in &paths {
        remove_file(old)?;
    }
    if let Some(directory) = paths.get(1).and_then(|path| path.parent()) {
        let _ = std::fs::remove_dir(directory);
    }
    Ok(())
}

pub(super) fn install(path: &Path) -> io::Result<()> {
    install_with_probe(path, legacy_cli)
}

fn install_with_probe(path: &Path, probe: impl FnOnce() -> io::Result<bool>) -> io::Result<()> {
    // Validate every config before changing any installed connector. The modern
    // definition precedes the legacy fallback in the configured package.
    let files = configs(path)?;
    preflight(path)?;
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("missing plugin directory"))?;
    remove_file(&directory.join("tui.ts"))?;
    write_current(&directory.join("server.ts"), SERVER)?;
    let tui = tui_path(path).ok_or_else(|| io::Error::other("missing TUI directory"))?;
    write_current(&tui, TUI)?;
    write_current(&directory.join("index.ts"), PLUGIN)?;
    // The transitional v1 loader requires main; no name/exports means v2
    // still chooses server.ts by convention rather than importing index.ts.
    write_current(&directory.join("package.json"), MANIFEST)?;
    // The auto-discovered directory becomes TUI-only before the configured
    // server definition appears, so v2 cannot activate duplicate server IDs.
    for old in old_paths(path).iter().skip(1) {
        remove_file(old)?;
    }
    if let Err(error) = register(&files, true) {
        if files.target_write_denied.get()
            && files[selected(&files)].original != files[selected(&files)].rendered()
            && configs(path)?[selected(&files)].managed_count() == 0
        {
            let restored = configs(path)?;
            let key = fallback_key(&restored)?;
            let legacy = probe().map_err(|probe| {
                io::Error::new(
                    probe.kind(),
                    format!(
                        "cannot register OpenCode connector in {}: {error}; {probe}",
                        restored[selected(&restored)].path.display()
                    ),
                )
            })?;
            if key != fallback_key(&restored)? {
                return Err(io::Error::other(
                    "OpenCode installation changed during version probe; retry connector setup",
                ));
            }
            if legacy {
                write_current(&old_paths(path)[0], PLUGIN)?;
            } else {
                remove_old(path)?;
            }
            let record =
                serde_json::json!({"uniterm": super::MARKER, "legacy": legacy, "key": key});
            write_current(
                &path.with_file_name("readonly-fallback"),
                &serde_json::to_string(&record)?,
            )?;
            return Ok(());
        }
        return Err(error);
    }
    rollback_on_error(&files, remove_old(path))?;
    remove_file(&path.with_file_name("readonly-fallback"))
}

pub(super) fn toggle(installing: bool) -> Option<io::Result<()>> {
    let path = plugin_path()?;
    Some(if installing {
        install(&path)
    } else {
        (|| {
            let files = configs(&path)?;
            preflight(&path)?;
            register(&files, false)?;
            remove_old(&path)?;
            let directory = path
                .parent()
                .ok_or_else(|| io::Error::other("missing plugin directory"))?;
            // Remove the legacy fallback before server.ts so a concurrent v2
            // discovery never mistakes a v1 function for its modern definition.
            for name in [
                "index.ts",
                "server.ts",
                "tui.ts",
                "package.json",
                "registration.json",
                "readonly-fallback",
            ] {
                remove_file(&directory.join(name))?;
            }
            let _ = std::fs::remove_dir(directory);
            if let Some(tui) = tui_path(&path) {
                remove_file(&tui)?;
                if let Some(directory) = tui.parent() {
                    let _ = std::fs::remove_dir(directory);
                }
            }
            Ok(())
        })()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "uniterm-opencode-package-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn index(&self) -> PathBuf {
            self.0.join("uniterm-notify/index.ts")
        }

        fn write(&self, name: &str, text: &str) {
            super::super::write_atomic(&self.0.join(name), text).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn package_registration_preserves_jsonc_precedence_and_old_layout_migration() {
        let fixture = Fixture::new();
        let lower = "{\n  // keep lower user settings\n  \"plugin\": [\"lower-plugin\"],\n  \"model\": \"user/model\"\n}\n";
        let upper = "{\n  // keep selected user settings\n  \"plugin\": [\"upper-plugin\",], // keep this comment\n  \"instructions\": [\"a;界.md\"],\n}\n";
        fixture.write("config.json", lower);
        fixture.write("opencode.jsonc", upper);
        fixture.write("plugins/uniterm-notify.ts", PLUGIN);
        fixture.write("plugins/uniterm-notify/index.ts", SERVER);
        fixture.write("plugins/uniterm-notify/tui.ts", TUI);
        fixture.write("plugins/unrelated.ts", "// user plugin");
        install(&fixture.index()).unwrap();
        assert!(registered(&configs(&fixture.index()).unwrap()));
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("config.json")).unwrap(),
            lower
        );
        assert!(!fixture.0.join("opencode.json").exists());
        let updated = std::fs::read_to_string(fixture.0.join("opencode.jsonc")).unwrap();
        for retained in [
            "// keep selected user settings",
            "// keep this comment",
            "upper-plugin",
            "a;界.md",
        ] {
            assert!(updated.contains(retained), "lost {retained}: {updated}");
        }
        assert_eq!(updated.matches(SPEC).count(), 1);
        assert!(old_paths(&fixture.index())
            .iter()
            .all(|path| !path.exists()));
        assert_eq!(std::fs::read_to_string(fixture.index()).unwrap(), PLUGIN);
        assert_eq!(
            std::fs::read_to_string(tui_path(&fixture.index()).unwrap()).unwrap(),
            TUI
        );
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("plugins/unrelated.ts")).unwrap(),
            "// user plugin"
        );
        let metadata = std::fs::metadata(fixture.0.join("opencode.jsonc"))
            .unwrap()
            .modified()
            .unwrap();
        register(&configs(&fixture.index()).unwrap(), true).unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("opencode.jsonc")).unwrap(),
            updated
        );
        assert_eq!(
            std::fs::metadata(fixture.0.join("opencode.jsonc"))
                .unwrap()
                .modified()
                .unwrap(),
            metadata
        );
        register(&configs(&fixture.index()).unwrap(), false).unwrap();
        let removed = std::fs::read_to_string(fixture.0.join("opencode.jsonc")).unwrap();
        assert!(!removed.contains(SPEC));
        assert!(removed.contains("upper-plugin"));
        assert!(removed.contains("// keep this comment"));
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("config.json")).unwrap(),
            lower
        );
    }

    #[test]
    fn registration_follows_a_new_effective_array_without_copying_user_plugins() {
        let fixture = Fixture::new();
        fixture.write("config.json", "{\"plugin\":[\"lower-plugin\"]}\n");
        install(&fixture.index()).unwrap();
        assert!(std::fs::read_to_string(fixture.0.join("config.json"))
            .unwrap()
            .contains(SPEC));
        fixture.write("opencode.jsonc", "{\"plugin\":[\"selected-plugin\"]}\n");
        assert!(!registered(&configs(&fixture.index()).unwrap()));
        install(&fixture.index()).unwrap();
        let lower = std::fs::read_to_string(fixture.0.join("config.json")).unwrap();
        let upper = std::fs::read_to_string(fixture.0.join("opencode.jsonc")).unwrap();
        assert!(lower.contains("lower-plugin"));
        assert!(!lower.contains(SPEC));
        assert!(upper.contains("selected-plugin"));
        assert!(!upper.contains("lower-plugin"));
        assert_eq!(upper.matches(SPEC).count(), 1);
        assert!(registered(&configs(&fixture.index()).unwrap()));
    }

    #[test]
    fn invalid_config_leaves_the_existing_connector_and_user_bytes_untouched() {
        for invalid in [
            "{ broken",
            "[]",
            "{\"plugin\":false}",
            "{\"plugin\":[],\"plugin\":[]}",
            "{plugin:[]}",
            "{\"a\":1 \"plugin\":[]}",
            "{\"plugin\":['user']}",
        ] {
            let fixture = Fixture::new();
            fixture.write("opencode.json", invalid);
            fixture.write("plugins/uniterm-notify.ts", PLUGIN);
            assert!(install(&fixture.index()).is_err(), "accepted {invalid}");
            assert_eq!(
                std::fs::read_to_string(fixture.0.join("opencode.json")).unwrap(),
                invalid
            );
            assert_eq!(
                std::fs::read_to_string(fixture.0.join("plugins/uniterm-notify.ts")).unwrap(),
                PLUGIN
            );
            assert!(!fixture.index().exists());
        }
    }

    #[test]
    fn fresh_registration_uses_a_config_read_by_both_generations() {
        let fixture = Fixture::new();
        install(&fixture.index()).unwrap();
        assert!(!fixture.0.join("config.json").exists());
        assert!(fixture.0.join("opencode.json").exists());
        assert!(!fixture.0.join("opencode.jsonc").exists());
        assert!(registered(&configs(&fixture.index()).unwrap()));
        // V2's automatic directory owns only the local TUI; it cannot be
        // activated twice as a configured and discovered server plugin.
        assert!(!fixture.0.join("plugins/uniterm-notify/index.ts").exists());
        assert!(!fixture.0.join("plugins/uniterm-notify/server.ts").exists());
        assert!(tui_path(&fixture.index()).unwrap().exists());
        // Transitional v1 requires main, while v2 must retain conventional
        // server.ts resolution rather than follow named package exports.
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(fixture.0.join("uniterm-notify/package.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["main"], "./index.ts");
        assert!(manifest.get("name").is_none());
        assert!(manifest.get("exports").is_none());
    }

    #[test]
    fn every_global_precedence_combination_preserves_the_effective_user_array() {
        for mask in 0..8 {
            let fixture = Fixture::new();
            for (index, name) in CONFIGS.iter().enumerate() {
                if mask & (1 << index) != 0 {
                    fixture.write(name, &format!("{{\"plugin\":[\"user-{index}\"]}}\n"));
                }
            }
            install(&fixture.index()).unwrap();
            let files = configs(&fixture.index()).unwrap();
            assert!(registered(&files), "mask {mask}");
            let target = selected(&files);
            if mask != 0 {
                assert!(files[target]
                    .root
                    .to_string()
                    .contains(&format!("user-{target}")));
            }
            for (index, file) in files.iter().enumerate() {
                if mask & (1 << index) != 0 && index != target {
                    assert_eq!(
                        file.original,
                        format!("{{\"plugin\":[\"user-{index}\"]}}\n")
                    );
                }
            }
        }
    }

    #[test]
    fn tuple_registration_and_unrelated_comment_only_configs_are_unchanged() {
        let fixture = Fixture::new();
        let comment = "// unrelated config stays byte-identical\r\n";
        fixture.write("opencode.json", comment);
        let configured =
            format!("{{\r\n\t\"plugin\": [[\"{SPEC}\", {{\"keep\":true}}],],\r\n}}\r\n");
        fixture.write("opencode.jsonc", &configured);
        install(&fixture.index()).unwrap();
        assert!(registered(&configs(&fixture.index()).unwrap()));
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("opencode.json")).unwrap(),
            comment
        );
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("opencode.jsonc")).unwrap(),
            configured
        );
        let index_time = std::fs::metadata(fixture.index())
            .unwrap()
            .modified()
            .unwrap();
        install(&fixture.index()).unwrap();
        assert_eq!(
            std::fs::metadata(fixture.index())
                .unwrap()
                .modified()
                .unwrap(),
            index_time
        );
    }

    #[test]
    fn uninstall_removes_only_owned_empty_arrays_and_preserves_config_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        for original in ["{\r\n\t\"theme\":\"界\"\r\n}\r\n", "{\"plugin\":[]}"] {
            let fixture = Fixture::new();
            fixture.write("dotfile.jsonc", original);
            std::fs::set_permissions(
                fixture.0.join("dotfile.jsonc"),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            symlink("dotfile.jsonc", fixture.0.join("opencode.jsonc")).unwrap();
            install(&fixture.index()).unwrap();
            let owned = !original.contains("plugin");
            let files = configs(&fixture.index()).unwrap();
            assert_eq!(files[2].owns_array(), owned);
            fixture.write("config.json", "{\"plugin\":[\"user-added-later\"]}");
            register(&configs(&fixture.index()).unwrap(), false).unwrap();
            let files = configs(&fixture.index()).unwrap();
            assert_eq!(files[2].plugins().is_none(), owned);
            assert_eq!(selected(&files), if owned { 0 } else { 2 });
            assert!(std::fs::symlink_metadata(fixture.0.join("opencode.jsonc"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                std::fs::metadata(fixture.0.join("dotfile.jsonc"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn failed_second_config_write_restores_first_without_overwriting_external_edits() {
        let fixture = Fixture::new();
        fixture.write("config.json", &format!("{{\"plugin\":[\"{SPEC}\"]}}"));
        fixture.write("opencode.jsonc", "{\"plugin\":[\"user\"]}");
        let files = configs(&fixture.index()).unwrap();
        fixture.write("config.json", "{\"plugin\":[\"external-edit\"]}");
        assert!(register(&files, true).is_err());
        assert_eq!(
            std::fs::read_to_string(&files[2].path).unwrap(),
            files[2].original
        );
        assert!(std::fs::read_to_string(&files[0].path)
            .unwrap()
            .contains("external-edit"));
    }

    #[test]
    fn strict_json_remains_json_and_ownership_survives_upstream_reserialization() {
        let fixture = Fixture::new();
        fixture.write("opencode.json", "{\"model\":\"user/model\"}\n");
        install(&fixture.index()).unwrap();
        let config = fixture.0.join("opencode.json");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        std::fs::write(&config, serde_json::to_string_pretty(&value).unwrap()).unwrap();
        fixture.write("config.json", "{\"plugin\":[\"user-added-later\"]}");
        register(&configs(&fixture.index()).unwrap(), false).unwrap();
        let files = configs(&fixture.index()).unwrap();
        assert!(files[1].plugins().is_none());
        assert_eq!(selected(&files), 0);
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
        assert_eq!(value["model"], "user/model");
    }

    #[test]
    fn pending_ownership_recovers_from_observed_config_without_claiming_user_arrays() {
        for original in [
            "{}".to_owned(),
            format!("{{\"plugin\":[\"{SPEC}\"]}}"),
            "{\"plugin\":[]}".to_owned(),
        ] {
            let fixture = Fixture::new();
            fixture.write("opencode.json", &original);
            fixture.write("uniterm-notify/registration.json", "{\"uniterm\":\"uniterm://cli-agent\",\"arrays\":[\"opencode.json\"],\"phase\":\"pending\"}");
            install(&fixture.index()).unwrap();
            let files = configs(&fixture.index()).unwrap();
            assert!(!files.ownership_pending());
            assert_eq!(
                status_at(&fixture.index()),
                super::super::ConnectorStatus::Installed
            );
            register(&files, false).unwrap();
            let files = configs(&fixture.index()).unwrap();
            assert_eq!(files[1].plugins().is_some(), original == "{\"plugin\":[]}");
        }
    }

    #[test]
    fn leading_comments_and_bom_survive_config_registration() {
        for prefix in ["// user heading\n", "\u{feff}// user heading\r\n"] {
            let fixture = Fixture::new();
            fixture.write("opencode.jsonc", &format!("{prefix}{{\"model\":\"界\"}}\n"));
            install(&fixture.index()).unwrap();
            let updated = std::fs::read_to_string(fixture.0.join("opencode.jsonc")).unwrap();
            assert!(updated.starts_with(prefix));
            assert!(registered(&configs(&fixture.index()).unwrap()));
        }
    }

    #[test]
    fn immutable_config_retains_version_specific_installation_without_repeated_probes() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        // SAFETY: geteuid has no arguments or preconditions; root cannot
        // exercise a permission-denied filesystem fixture.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        for legacy in [true, false] {
            let fixture = Fixture::new();
            fixture.write("store/config.json", "{\"plugin\":[\"user-plugin\"]}");
            symlink("store/config.json", fixture.0.join("opencode.json")).unwrap();
            let store = fixture.0.join("store");
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o555)).unwrap();
            let result = install_with_probe(&fixture.index(), || Ok(legacy));
            let status = status_at(&fixture.index());
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();
            result.unwrap();
            assert_eq!(status, super::super::ConnectorStatus::Installed);
            assert_eq!(
                status_at(&fixture.index()),
                super::super::ConnectorStatus::Outdated
            );
            assert_eq!(fixture.0.join("plugins/uniterm-notify.ts").exists(), legacy);
            assert_eq!(
                std::fs::read_to_string(store.join("config.json")).unwrap(),
                "{\"plugin\":[\"user-plugin\"]}"
            );
            // A later explicit refresh can migrate once the config is writable.
            install_with_probe(&fixture.index(), || {
                panic!("normal installation must not probe")
            })
            .unwrap();
            assert!(registered(&configs(&fixture.index()).unwrap()));
            assert!(!fixture.index().with_file_name("readonly-fallback").exists());
            assert!(!fixture.0.join("plugins/uniterm-notify.ts").exists());
        }
    }

    #[test]
    fn failed_legacy_cleanup_rolls_back_registration_and_retry_installs_once() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no arguments or preconditions. Root bypasses the
        // permission fault exercised here; the transactional-write test does not.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let fixture = Fixture::new();
        fixture.write("plugins/uniterm-notify.ts", PLUGIN);
        // Existing TUI means installation needs no writes in this directory
        // before attempting to unlink the legacy entrypoint.
        fixture.write("plugins/uniterm-notify/tui.ts", TUI);
        let plugins = fixture.0.join("plugins");
        std::fs::set_permissions(&plugins, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = install(&fixture.index());
        std::fs::set_permissions(&plugins, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert!(plugins.join("uniterm-notify.ts").exists());
        assert!(!registered(&configs(&fixture.index()).unwrap()));
        install(&fixture.index()).unwrap();
        assert!(!plugins.join("uniterm-notify.ts").exists());
        assert_eq!(
            status_at(&fixture.index()),
            super::super::ConnectorStatus::Installed
        );
    }

    #[test]
    fn loader_conflicts_are_preserved_and_owned_outside_tui_is_migrated() {
        for name in [
            "uniterm-notify/package.json",
            "plugins/uniterm-notify/index.js",
            "uniterm-notify/tui.ts",
        ] {
            let fixture = Fixture::new();
            install(&fixture.index()).unwrap();
            fixture.write(name, "user-owned");
            assert_ne!(
                status_at(&fixture.index()),
                super::super::ConnectorStatus::Installed
            );
            assert!(install(&fixture.index()).is_err());
            assert_eq!(
                std::fs::read_to_string(fixture.0.join(name)).unwrap(),
                "user-owned"
            );
        }
        let fixture = Fixture::new();
        fixture.write("uniterm-notify/tui.ts", TUI);
        install(&fixture.index()).unwrap();
        assert!(!fixture.0.join("uniterm-notify/tui.ts").exists());
        let legacy = fixture.0.join("plugins/uniterm-notify.ts");
        std::fs::write(&legacy, [0xff]).unwrap();
        assert_ne!(
            status_at(&fixture.index()),
            super::super::ConnectorStatus::Installed
        );
        assert!(install(&fixture.index()).is_err());
        assert_eq!(std::fs::read(legacy).unwrap(), [0xff]);
    }
}
