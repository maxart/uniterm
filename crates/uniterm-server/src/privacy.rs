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
        let mut file = std::fs::OpenOptions::new()
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
        let mut nonce = [0; 24];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        let encrypted = self
            .0
            .encrypt(
                &XNonce::from(nonce),
                chacha20poly1305::aead::Payload {
                    msg: plain,
                    aad: MAGIC,
                },
            )
            .map_err(|_| std::io::Error::other("state encryption failed"))?;
        let mut bytes = Vec::with_capacity(MAGIC.len() + 24 + encrypted.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&nonce);
        bytes.extend(encrypted);
        Ok(bytes)
    }

    fn open(&self, bytes: &[u8]) -> std::io::Result<Vec<u8>> {
        let bytes = bytes
            .strip_prefix(MAGIC)
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
                    aad: MAGIC,
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
            })
        {
            std::fs::remove_file(path)?;
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
}
