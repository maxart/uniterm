//! Optional authenticated encryption for local durable Workspace data.
//!
//! A caller-supplied owner-only key lives outside the state directory.
//! Protection is an explicit offline migration; a missing/wrong key or an
//! interrupted migration refuses startup instead of quarantining readable data.
//! See docs/27 and https://docs.rs/chacha20poly1305/ for the AEAD implementation.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use std::borrow::Cow;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MAGIC: &[u8] = b"UNITERM-PRIVATE-1\n";
const CHECK: &[u8] = b"Uniterm local state key verification v1";

struct Cipher(XChaCha20Poly1305);

impl Cipher {
    fn load(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no pointer arguments or preconditions.
        if !metadata.is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.len() != 32
        {
            return Err(std::io::Error::other(
                "state key must be a 32-byte regular file owned by this user with mode 0600",
            ));
        }
        if path
            .canonicalize()?
            .starts_with(crate::persist::state_dir().canonicalize()?)
        {
            return Err(std::io::Error::other(
                "state key must be stored outside the Uniterm state directory",
            ));
        }
        Self::read(file)
    }

    /// The per-Workspace inbox key of an unprotected Workspace. It is local
    /// by design, so only ownership and the private mode are checked.
    fn local(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no pointer arguments or preconditions.
        if !metadata.is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.len() != 32
        {
            return Err(std::io::Error::other("invalid inbox key file"));
        }
        Self::read(file)
    }

    fn read(mut file: std::fs::File) -> std::io::Result<Self> {
        let mut key = [0; 32];
        file.read_exact(&mut key)?;
        let cipher = XChaCha20Poly1305::new_from_slice(&key)
            .map_err(|_| std::io::Error::other("invalid state key"))?;
        // The cipher's zeroize feature clears its key on drop. This temporary
        // buffer is explicitly overwritten too, without printing its contents.
        for byte in &mut key {
            unsafe {
                std::ptr::write_volatile(byte, 0);
            }
        }
        Ok(Self(cipher))
    }

    fn seal(&self, plain: &[u8]) -> std::io::Result<Vec<u8>> {
        self.seal_as(MAGIC, plain)
    }

    fn open(&self, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        self.open_as(MAGIC, bytes)
    }

    /// The header doubles as associated data, so a record sealed for one
    /// purpose never authenticates as another.
    fn seal_as(&self, magic: &[u8], plain: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut nonce = [0; 24];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        let encrypted = self
            .0
            .encrypt(
                &XNonce::from(nonce),
                chacha20poly1305::aead::Payload {
                    msg: plain,
                    aad: magic,
                },
            )
            .map_err(|_| std::io::Error::other("state encryption failed"))?;
        let mut bytes = Vec::with_capacity(magic.len() + 24 + encrypted.len());
        bytes.extend_from_slice(magic);
        bytes.extend_from_slice(&nonce);
        bytes.extend(encrypted);
        Ok(bytes)
    }

    fn open_as(&self, magic: &[u8], bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        let bytes = bytes
            .strip_prefix(magic)
            .ok_or_else(|| std::io::Error::other("invalid encrypted state header"))?;
        let nonce: [u8; 24] = bytes
            .get(..24)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| std::io::Error::other("truncated encrypted state"))?;
        self.0
            .decrypt(
                &XNonce::from(nonce),
                chacha20poly1305::aead::Payload {
                    msg: &bytes[24..],
                    aad: magic,
                },
            )
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "state key is incorrect or encrypted data was damaged",
                )
            })
    }
}

fn cipher() -> std::io::Result<&'static Cipher> {
    static KEY: OnceLock<Result<Cipher, String>> = OnceLock::new();
    KEY.get_or_init(|| {
        let path = std::env::var_os("UNITERM_STATE_KEY_FILE").ok_or_else(|| {
            "Workspace is locked; set UNITERM_STATE_KEY_FILE to its external key file".to_string()
        })?;
        Cipher::load(Path::new(&path)).map_err(|e| e.to_string())
    })
    .as_ref()
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::PermissionDenied, error.clone()))
}

fn marker(name: &str) -> PathBuf {
    crate::persist::snapshot_path(name).with_extension("privacy")
}
fn pending(name: &str) -> PathBuf {
    crate::persist::snapshot_path(name).with_extension("privacy.pending")
}

/// Whether this Workspace explicitly enabled encrypted durable storage.
pub fn is_protected(name: &str) -> bool {
    marker(name).exists()
}

/// Fail before opening a server or repairing any data if its key is unavailable.
pub(crate) fn verify(name: &str) -> std::io::Result<()> {
    if pending(name).exists() {
        return Err(std::io::Error::other("Workspace protection migration was interrupted; rerun `ut privacy protect` with the same key"));
    }
    if is_protected(name) && cipher()?.open(&std::fs::read(marker(name))?)? != CHECK {
        return Err(std::io::Error::other("invalid Workspace protection marker"));
    }
    Ok(())
}

/// Encrypt a snapshot or cache only after protection was explicitly enabled.
pub(crate) fn encode(name: &str, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    if is_protected(name) {
        cipher()?.seal(bytes)
    } else {
        Ok(bytes.to_vec())
    }
}

/// Decode either legacy plaintext or an authenticated protected file.
pub(crate) fn decode(bytes: &[u8]) -> std::io::Result<Cow<'_, [u8]>> {
    if bytes.starts_with(MAGIC) {
        Ok(Cow::Owned(cipher()?.open(bytes)?))
    } else {
        Ok(Cow::Borrowed(bytes))
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| {
            [
                DIGITS[(b >> 4) as usize] as char,
                DIGITS[(b & 15) as usize] as char,
            ]
        })
        .collect()
}
fn unhex(text: &str) -> std::io::Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(std::io::Error::other("invalid encrypted record"));
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let nibble = |b: u8| (b as char).to_digit(16).map(|n| n as u8);
            Ok(
                nibble(pair[0]).ok_or_else(|| std::io::Error::other("invalid encrypted record"))?
                    * 16
                    + nibble(pair[1])
                        .ok_or_else(|| std::io::Error::other("invalid encrypted record"))?,
            )
        })
        .collect()
}

fn sealed_line(cipher: &Cipher, line: &str) -> std::io::Result<String> {
    // Older binaries recognize the future schema and refuse startup. They
    // must never misinterpret encryption as corruption and truncate the log.
    Ok(format!(
        "{{\"version\":4294967295,\"uniterm_encrypted\":\"{}\"}}\n",
        hex(&cipher.seal(line.as_bytes())?)
    ))
}

/// Each append is independently authenticated and keeps the streaming log shape.
pub(crate) fn encode_line(name: &str, line: &str) -> std::io::Result<String> {
    if is_protected(name) {
        sealed_line(cipher()?, line)
    } else {
        Ok(line.to_owned())
    }
}

/// Restore one protected log record without retaining lifetime plaintext.
pub(crate) fn decode_line(line: &str) -> std::io::Result<Cow<'_, str>> {
    if !line.starts_with("{\"version\":4294967295,\"uniterm_encrypted\":") {
        return Ok(Cow::Borrowed(line));
    }
    let value: serde_json::Value = serde_json::from_str(line).map_err(std::io::Error::other)?;
    let encoded = value["uniterm_encrypted"]
        .as_str()
        .ok_or_else(|| std::io::Error::other("invalid encrypted record"))?;
    String::from_utf8(cipher()?.open(&unhex(encoded)?)?)
        .map(Cow::Owned)
        .map_err(std::io::Error::other)
}

fn atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("protect.tmp");
    let mut file = crate::persist::open_private_append(&tmp)?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    crate::persist::sync_parent_directory(path)
}

/// Generate a key at a caller-selected path without ever overwriting a key.
pub fn generate_key(path: &Path) -> std::io::Result<()> {
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    for byte in &mut bytes {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    Ok(())
}

/// Encrypt a stopped Workspace while the caller holds its ordinary Workspace lock.
/// An interrupted migration is retryable and prevents normal startup meanwhile.
pub fn protect(name: &str, key_file: &Path) -> std::io::Result<()> {
    crate::persist::ensure_private_dir(&crate::persist::state_dir())?;
    let key = Cipher::load(key_file)?;
    for path in [marker(name), pending(name)] {
        if path.exists() && key.open(&std::fs::read(path)?)? != CHECK {
            return Err(std::io::Error::other("incorrect protection key"));
        }
    }
    atomic(&pending(name), &key.seal(CHECK)?)?;
    let root = crate::persist::state_dir();
    for item in std::fs::read_dir(&root)? {
        let path = item?.path();
        let Some(file) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !(file == format!("{name}.log")
            || file.starts_with(&format!("{name}.log."))
            || file == format!("{name}.snap")
            || file.starts_with(&format!("{name}.snap.")))
        {
            continue;
        }
        if file == format!("{name}.log") {
            protect_lines(&path, &key)?;
        } else {
            let bytes = std::fs::read(&path)?;
            if bytes.starts_with(MAGIC) {
                key.open(&bytes)?;
            } else {
                atomic(&path, &key.seal(&bytes)?)?;
            }
        }
    }
    // Quarantined archives still contain retained private history too.
    for item in std::fs::read_dir(crate::persist::state_dir())? {
        let item = item?;
        let file = item.file_name();
        let file = file.to_string_lossy();
        if item.file_type()?.is_dir()
            && (file == format!("{name}.history")
                || file.starts_with(&format!("{name}.history.corrupt-")))
        {
            for item in std::fs::read_dir(item.path())? {
                let path = item?.path();
                if path.is_file() {
                    protect_lines(&path, &key)?;
                }
            }
        }
    }
    let catalog = crate::workspace_catalog::privacy_path(name);
    if catalog.exists() {
        protect_lines(&catalog, &key)?;
    }
    let compact = catalog.with_extension("jsonl.compact.tmp");
    if compact.exists() {
        protect_lines(&compact, &key)?;
    }
    let cache = crate::persist::snapshot_path(name).with_extension("timeline");
    if cache.exists() {
        std::fs::remove_dir_all(cache)?;
    }
    protect_inbox(name, &key)?;
    atomic(&marker(name), &key.seal(CHECK)?)?;
    std::fs::remove_file(pending(name))?;
    crate::persist::sync_parent_directory(&marker(name))
}

fn protect_lines(path: &Path, key: &Cipher) -> std::io::Result<()> {
    use std::io::BufRead;
    let tmp = path.with_extension("protect.tmp");
    let mut output = crate::persist::open_private_append(&tmp)?;
    output.set_len(0)?;
    for line in std::io::BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        if line.starts_with("{\"version\":4294967295,\"uniterm_encrypted\":") {
            let value: serde_json::Value =
                serde_json::from_str(&line).map_err(std::io::Error::other)?;
            key.open(&unhex(
                value["uniterm_encrypted"].as_str().unwrap_or_default(),
            )?)?;
            output.write_all(line.as_bytes())?;
            output.write_all(b"\n")?;
        } else {
            output.write_all(sealed_line(key, &line)?.as_bytes())?;
        }
    }
    output.sync_all()?;
    std::fs::rename(tmp, path)?;
    crate::persist::sync_parent_directory(path)
}

// --- Project inbox store (docs/29) -------------------------------------------
//
// Inbox bodies are never written in plaintext and never enter the Workspace
// event stream or snapshot. A protected Workspace seals them with its external
// key. Any other Workspace seals them with a random key kept beside the store
// at mode 0600: that keeps bodies out of searches, partial copies and tools
// that read state files, and lets a clear destroy every stale copy by deleting
// the key. It is not protection against a reader of the whole state directory;
// `ut privacy protect` provides that.

const INBOX_MAGIC: &[u8] = b"UNITERM-INBOX-1\n";
/// Largest sealed store accepted from disk; the board's own budgets are lower.
const INBOX_FILE_LIMIT: u64 = 32 * 1024 * 1024;

fn inbox_directory(name: &str) -> PathBuf {
    crate::persist::state_dir()
        .join("inbox")
        .join(format!("{name}.inbox"))
}

pub(crate) fn inbox_file(name: &str, file: &str) -> PathBuf {
    inbox_directory(name).join(file)
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn write_inbox_file(name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let path = inbox_file(name, "board");
    let tmp = inbox_file(name, "board.tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(bytes)?;
    crate::persist::sync_file_for_crash(&file)?;
    std::fs::rename(tmp, &path)?;
    crate::persist::sync_parent_directory(&path)
}

/// Load the local inbox key, creating one for the first stored post. A key
/// already present must pass verification before the board can be replaced.
fn inbox_local_key(name: &str, create: bool) -> std::io::Result<Cipher> {
    let path = inbox_file(name, "key");
    match Cipher::local(&path) {
        Ok(cipher) => return Ok(cipher),
        Err(error) => {
            if !create || inbox_file(name, "board").try_exists()? {
                return Err(error);
            }
            if error.kind() != std::io::ErrorKind::NotFound {
                // Older writers created the final key directly. Recover an
                // interrupted first write only when there is no sealed copy.
                let meta = std::fs::symlink_metadata(&path)?;
                // SAFETY: geteuid has no arguments or preconditions.
                if !meta.is_file()
                    || meta.len() >= 32
                    || meta.mode() & 0o077 != 0
                    || meta.uid() != unsafe { libc::geteuid() }
                    || inbox_file(name, "board.tmp").try_exists()?
                {
                    return Err(error);
                }
            }
        }
    }
    // A crash or ENOSPC can leave a partial temporary key. The Workspace
    // lock excludes other writers, and no board references this filename.
    let tmp = inbox_file(name, "key.tmp");
    remove_if_present(&tmp)?;
    generate_key(&tmp)?;
    std::fs::rename(tmp, &path)?;
    crate::persist::sync_parent_directory(&path)?;
    Cipher::local(&path)
}

/// Seal and atomically replace the Workspace's stored inbox. `protected`
/// selects the external Workspace key; the caller remembers it across a
/// rename, when the marker of the new name may not exist yet.
pub(crate) fn save_inbox(name: &str, protected: bool, plain: &[u8]) -> std::io::Result<()> {
    crate::persist::ensure_private_dir(&inbox_directory(name))?;
    let sealed = if protected {
        cipher()?.seal_as(INBOX_MAGIC, plain)?
    } else {
        inbox_local_key(name, true)?.seal_as(INBOX_MAGIC, plain)?
    };
    write_inbox_file(name, &sealed)?;
    if protected {
        remove_if_present(&inbox_file(name, "key"))?;
    }
    Ok(())
}

/// Read and authenticate the stored inbox; `None` when nothing is stored.
/// Callers preserve any existing board when authentication or I/O fails.
pub(crate) fn load_inbox(name: &str, protected: bool) -> std::io::Result<Option<Vec<u8>>> {
    let Some(sealed) = read_inbox_bytes(name)? else {
        return Ok(None);
    };
    if protected {
        cipher()?.open_as(INBOX_MAGIC, &sealed)
    } else {
        inbox_local_key(name, false)?.open_as(INBOX_MAGIC, &sealed)
    }
    .map(Some)
}

fn read_inbox_bytes(name: &str) -> std::io::Result<Option<Vec<u8>>> {
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(inbox_file(name, "board"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no pointer arguments or preconditions.
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.len() > INBOX_FILE_LIMIT
    {
        return Err(std::io::Error::other("invalid inbox store file"));
    }
    let mut sealed = Vec::with_capacity(metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(INBOX_FILE_LIMIT + 1)
        .read_to_end(&mut sealed)?;
    if sealed.len() as u64 > INBOX_FILE_LIMIT {
        return Err(std::io::Error::other("inbox store exceeds its size limit"));
    }
    Ok(Some(sealed))
}

/// Remove the stored inbox and its local key. Deleting the key also makes
/// any stale copy of the sealed file unreadable.
pub(crate) fn erase_inbox(name: &str) -> std::io::Result<()> {
    let directory = inbox_directory(name);
    if !directory.try_exists()? {
        return Ok(());
    }
    for file in ["board.tmp", "board", "key.tmp", "key"] {
        remove_if_present(&directory.join(file))?;
    }
    std::fs::remove_dir(&directory)?;
    crate::persist::sync_parent_directory(&directory)
}

/// Whether a sealed inbox exists for this Workspace (test and CLI checks).
pub fn inbox_exists(name: &str) -> bool {
    inbox_file(name, "board").exists()
}

/// An entire Inbox directory reserves its name, including a key left by
/// an interrupted first write. A dangling symlink also occupies the path.
pub(crate) fn inbox_name_taken(name: &str) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(inbox_directory(name)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Verify both names while their Workspace claims are held, before moving
/// any files. A marker at the destination belongs to a different Workspace.
pub fn verify_rename(old: &str, new: &str) -> std::io::Result<()> {
    if pending(old).try_exists()? {
        return Err(std::io::Error::other(
            "Finish protection migration before renaming this Workspace",
        ));
    }
    if marker(new).try_exists()? || pending(new).try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "destination already carries a Workspace protection marker",
        ));
    }
    if inbox_name_taken(new)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "destination already has saved state in its Inbox directory",
        ));
    }
    Ok(())
}

/// Release the old protection marker once the new owner has its identical
/// marker and no saved or quarantined files remain under the old name.
pub fn finish_rename(old: &str, new: &str) -> std::io::Result<()> {
    let source = marker(old);
    if !source.try_exists()? {
        return Ok(());
    }
    if std::fs::read(&source)? != std::fs::read(marker(new))? {
        return Err(std::io::Error::other(
            "renamed Workspace protection marker does not match its source",
        ));
    }
    let catalog = crate::workspace_catalog::privacy_path(old);
    if inbox_name_taken(old)?
        || pending(old).try_exists()?
        || catalog.try_exists()?
        || catalog.with_extension("jsonl.compact.tmp").try_exists()?
        || catalog.with_extension("protect.tmp").try_exists()?
    {
        return Ok(());
    }
    let log = format!("{old}.log");
    let snapshot = format!("{old}.snap");
    let history = format!("{old}.history");
    let prefixes = [
        format!("{log}."),
        format!("{snapshot}."),
        format!("{history}.corrupt-"),
    ];
    for item in std::fs::read_dir(crate::persist::state_dir())? {
        let name = item?.file_name();
        let name = name.to_string_lossy();
        if name == log
            || name == snapshot
            || name == history
            || prefixes.iter().any(|prefix| name.starts_with(prefix))
        {
            return Ok(());
        }
    }
    std::fs::remove_file(&source)?;
    crate::persist::sync_parent_directory(&source)
}

/// Move a Workspace's stored inbox to its new name. A running Workspace's
/// runtime owns its store and moves it when its endpoints rename; the CLI
/// calls this for a stopped Workspace. It is deliberately not part of
/// [`rename`]: that runs while a live runtime may still write the old name,
/// and a store recreated there would then fail the event-stream rename.
pub fn rename_inbox(old: &str, new: &str) -> std::io::Result<()> {
    let from = inbox_directory(old);
    let to = inbox_directory(new);
    if from == to || !inbox_name_taken(old)? {
        return Ok(());
    }
    if inbox_name_taken(new)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "destination already has an Inbox",
        ));
    }
    // Board and key stay together across interruption at any point in the rename.
    std::fs::rename(&from, &to)?;
    if let Err(error) = crate::persist::sync_parent_directory(&to) {
        std::fs::rename(&to, &from).map_err(|rollback| {
            std::io::Error::other(format!(
                "Inbox rename sync failed ({error}), rollback failed: {rollback}"
            ))
        })?;
        return Err(error);
    }
    Ok(())
}

/// Re-seal a stopped Workspace's inbox under its external key. A store that
/// neither key opens stops the migration and retains the original files.
fn protect_inbox(name: &str, key: &Cipher) -> std::io::Result<()> {
    let Some(sealed) = read_inbox_bytes(name)? else {
        return erase_inbox(name);
    };
    if key.open_as(INBOX_MAGIC, &sealed).is_err() {
        match inbox_local_key(name, false).and_then(|local| local.open_as(INBOX_MAGIC, &sealed)) {
            Ok(plain) => write_inbox_file(name, &key.seal_as(INBOX_MAGIC, &plain)?)?,
            Err(error) => return Err(error),
        }
    }
    remove_if_present(&inbox_file(name, "key"))
}

/// Rename protection together with the authoritative Workspace event stream.
pub(crate) fn rename(old: &str, new: &str) -> std::io::Result<()> {
    if pending(old).exists() {
        return Err(std::io::Error::other(
            "Finish protection migration before renaming this Workspace",
        ));
    }
    if marker(old).exists() {
        // Keep the old verification marker until all files have moved. A
        // failed multi-file rename must never leave ciphertext looking plain.
        atomic(&marker(new), &std::fs::read(marker(old))?)?;
    }
    let cache = crate::persist::snapshot_path(old).with_extension("timeline");
    if cache.exists() {
        std::fs::remove_dir_all(cache)?;
    }
    Ok(())
}

/// Forget derived history and key-verification metadata with a stopped Workspace.
pub fn forget(name: &str) -> std::io::Result<()> {
    erase_inbox(name)?;
    for path in [marker(name), pending(name)] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    let cache = crate::persist::snapshot_path(name).with_extension("timeline");
    if cache.exists() {
        std::fs::remove_dir_all(cache)?;
    }
    for item in std::fs::read_dir(crate::persist::state_dir())? {
        let path = item?.path();
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|file| {
                file.starts_with(&format!("{name}.log."))
                    || file.starts_with(&format!("{name}.snap."))
                    || file.starts_with(&format!("{name}.history.corrupt-"))
            })
        {
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
        }
    }
    let catalog = crate::workspace_catalog::privacy_path(name);
    for path in [
        catalog.with_extension("jsonl.compact.tmp"),
        catalog.with_extension("protect.tmp"),
    ] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticated_state_rejects_tampering_and_wrong_keys() {
        let cipher = Cipher(XChaCha20Poly1305::new_from_slice(&[7; 32]).unwrap());
        let other = Cipher(XChaCha20Poly1305::new_from_slice(&[8; 32]).unwrap());
        let plain = b"private prompt and session";
        let mut bytes = cipher.seal(plain).unwrap();
        assert!(!bytes.windows(plain.len()).any(|s| s == plain));
        assert_eq!(cipher.open(&bytes).unwrap(), plain);
        assert!(other.open(&bytes).is_err());
        *bytes.last_mut().unwrap() ^= 1;
        assert!(cipher.open(&bytes).is_err());
        assert!(cipher.open(MAGIC).is_err());
        assert_eq!(unhex(&hex(plain)).unwrap(), plain);
        assert!(unhex("z0").is_err());
    }

    #[test]
    fn migration_is_retryable_and_covers_snapshot_and_catalog_copies() {
        crate::persist::ensure_private_dir(&crate::persist::state_dir()).unwrap();
        let name = format!("privacy-migration-{}", std::process::id());
        let key_path = std::env::temp_dir().join(format!("uniterm-key-{}", std::process::id()));
        let _ = std::fs::remove_file(&key_path);
        generate_key(&key_path).unwrap();
        assert!(
            generate_key(&key_path).is_err(),
            "keygen must not replace a key"
        );
        let snapshot = crate::persist::snapshot_path(&name);
        std::fs::write(&snapshot, b"secret snapshot").unwrap();
        let log = snapshot.with_extension("log");
        std::fs::write(&log, b"{\"secret\":\"prompt\"}\n").unwrap();
        let backup = snapshot.with_extension("snap.corrupt-test");
        std::fs::write(&backup, b"secret backup").unwrap();
        let catalog = crate::workspace_catalog::privacy_path(&name);
        crate::persist::ensure_private_dir(catalog.parent().unwrap()).unwrap();
        std::fs::write(&catalog, b"{\"secret_catalog\":true}\n").unwrap();
        let compact = catalog.with_extension("jsonl.compact.tmp");
        std::fs::write(&compact, b"{\"secret_catalog\":true}\n").unwrap();
        protect(&name, &key_path).unwrap();
        assert!(is_protected(&name));
        assert!(!pending(&name).exists());
        let key = Cipher::load(&key_path).unwrap();
        assert_eq!(
            key.open(&std::fs::read(&snapshot).unwrap()).unwrap(),
            b"secret snapshot"
        );
        assert!(!std::fs::read_to_string(&log).unwrap().contains("prompt"));
        protect(&name, &key_path).unwrap();
        assert_eq!(
            key.open(&std::fs::read(&snapshot).unwrap()).unwrap(),
            b"secret snapshot"
        );
        assert_eq!(
            key.open(&std::fs::read(&backup).unwrap()).unwrap(),
            b"secret backup"
        );
        for path in [&catalog, &compact] {
            assert!(!std::fs::read_to_string(path)
                .unwrap()
                .contains("secret_catalog"));
        }
        forget(&name).unwrap();
        std::fs::remove_file(snapshot).unwrap();
        std::fs::remove_file(log).unwrap();
        std::fs::remove_file(key_path).unwrap();
        std::fs::remove_file(catalog).unwrap();
        assert!(!compact.exists() && !backup.exists());
    }

    #[test]
    fn interrupted_inbox_key_creation_retries_without_replacing_a_stored_boards_key() {
        let name = format!("privacy-short-key-{}", std::process::id());
        crate::persist::ensure_private_dir(&inbox_directory(&name)).unwrap();
        let key = inbox_file(&name, "key");
        let tmp = inbox_file(&name, "key.tmp");
        let board = inbox_file(&name, "board");
        // Both the current temporary pathname and a legacy partial final key
        // may be left by an interrupted first write.
        for partial in [&tmp, &key] {
            std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(partial)
                .unwrap()
                .write_all(b"short")
                .unwrap();
        }
        save_inbox(&name, false, b"saved after retry").unwrap();
        assert!(!tmp.exists());
        assert_eq!(std::fs::metadata(&key).unwrap().len(), 32);
        assert_eq!(
            load_inbox(&name, false).unwrap().unwrap(),
            b"saved after retry"
        );
        let original = std::fs::read(&board).unwrap();
        std::fs::write(&key, b"short").unwrap();
        assert!(save_inbox(&name, false, b"unrelated change").is_err());
        assert_eq!(std::fs::read(&key).unwrap(), b"short");
        assert_eq!(std::fs::read(board).unwrap(), original);
        erase_inbox(&name).unwrap();
    }

    #[test]
    fn rename_releases_only_the_marker_of_a_fully_moved_workspace() {
        let old = format!("privacy-finish-{}", std::process::id());
        let new = format!("{old}-moved");
        crate::persist::ensure_private_dir(&crate::persist::state_dir()).unwrap();
        std::fs::write(marker(&old), b"old marker").unwrap();
        std::fs::write(pending(&old), b"migration").unwrap();
        assert!(verify_rename(&old, &new)
            .unwrap_err()
            .to_string()
            .contains("protection migration"));
        std::fs::remove_file(pending(&old)).unwrap();
        verify_rename(&old, &new).unwrap();
        rename(&old, &new).unwrap();
        assert!(verify_rename(&old, &new)
            .unwrap_err()
            .to_string()
            .contains("protection marker"));
        let quarantine = crate::persist::snapshot_path(&old).with_extension("log.corrupt-1");
        std::fs::write(&quarantine, b"recovery data").unwrap();
        finish_rename(&old, &new).unwrap();
        assert!(is_protected(&old));
        std::fs::remove_file(quarantine).unwrap();
        finish_rename(&old, &new).unwrap();
        assert!(!is_protected(&old));
        assert!(is_protected(&new));
        std::fs::remove_file(marker(&new)).unwrap();
    }

    #[test]
    fn inbox_store_is_sealed_private_and_follows_protect_rename_and_forget() {
        crate::persist::ensure_private_dir(&crate::persist::state_dir()).unwrap();
        let name = format!("privacy-inbox-{}", std::process::id());
        let renamed = format!("{name}-renamed");
        let plain = b"{\"body\":\"inbox secret\"}";
        let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o777;
        let chmod = |path: &Path, mode| {
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
                .unwrap()
        };
        assert_eq!(load_inbox(&name, false).unwrap(), None);
        save_inbox(&name, false, plain).unwrap();
        let board = inbox_file(&name, "board");
        let key = inbox_file(&name, "key");
        assert_eq!(mode(&board), 0o600);
        assert_eq!(mode(&key), 0o600);
        assert_eq!(mode(board.parent().unwrap()), 0o700);
        assert!(!inbox_file(&name, "board.tmp").exists());
        let sealed = std::fs::read(&board).unwrap();
        assert!(sealed.starts_with(INBOX_MAGIC));
        assert!(!sealed.windows(12).any(|part| part == b"inbox secret"));
        assert_eq!(
            load_inbox(&name, false).unwrap().as_deref(),
            Some(&plain[..])
        );
        // The record is bound to its purpose: it is not Workspace state.
        let local = Cipher::local(&key).unwrap();
        assert!(local.open(&sealed).is_err());
        let mut as_state = MAGIC.to_vec();
        as_state.extend_from_slice(&sealed[INBOX_MAGIC.len()..]);
        assert!(local.open(&as_state).is_err());
        // Neither a readable store nor a readable key is trusted.
        chmod(&board, 0o640);
        assert!(load_inbox(&name, false).is_err());
        chmod(&board, 0o600);
        chmod(&key, 0o640);
        assert!(load_inbox(&name, false).is_err());
        chmod(&key, 0o600);

        // A dormant rename moves the store and key; a repeat changes nothing.
        rename_inbox(&name, &renamed).unwrap();
        assert!(!inbox_exists(&name) && !key.exists());
        rename_inbox(&name, &renamed).unwrap();
        // Renaming protection metadata never touches a store a live runtime owns.
        rename(&renamed, &name).unwrap();
        assert!(inbox_exists(&renamed) && !inbox_exists(&name));
        assert_eq!(
            load_inbox(&renamed, false).unwrap().as_deref(),
            Some(&plain[..])
        );

        // Protection re-seals under the external key and drops the local one.
        let key_path =
            std::env::temp_dir().join(format!("uniterm-inbox-key-{}", std::process::id()));
        let _ = std::fs::remove_file(&key_path);
        generate_key(&key_path).unwrap();
        let external = Cipher::load(&key_path).unwrap();
        for _ in 0..2 {
            protect(&renamed, &key_path).unwrap();
            assert!(!inbox_file(&renamed, "key").exists());
            let sealed = std::fs::read(inbox_file(&renamed, "board")).unwrap();
            assert_eq!(external.open_as(INBOX_MAGIC, &sealed).unwrap(), plain);
            assert!(external.open(&sealed).is_err());
        }
        forget(&renamed).unwrap();
        assert!(!inbox_exists(&renamed) && !is_protected(&renamed));

        // Failed authentication preserves the encrypted original. Restoring the
        // private key allows a retry without discarding the conversation.
        save_inbox(&name, false, plain).unwrap();
        let original = std::fs::read(&board).unwrap();
        let backup = key.with_extension("backup");
        std::fs::rename(&key, &backup).unwrap();
        assert!(protect(&name, &key_path).is_err());
        assert_eq!(std::fs::read(&board).unwrap(), original);
        std::fs::rename(&backup, &key).unwrap();
        protect(&name, &key_path).unwrap();
        assert_eq!(
            external
                .open_as(INBOX_MAGIC, &std::fs::read(&board).unwrap())
                .unwrap(),
            plain
        );
        forget(&name).unwrap();
        std::fs::remove_file(key_path).unwrap();
    }
}
