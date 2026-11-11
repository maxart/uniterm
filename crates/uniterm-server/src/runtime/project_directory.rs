//! Filesystem validation belongs on a blocking worker, never the mio loop.

use std::path::PathBuf;

/// Resolve a host path without changing the runtime process working directory.
pub(super) fn validate(input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() || input.len() > 4096 || input.contains('\0') {
        return Err("Enter a directory path of 1 to 4096 bytes without NUL characters".into());
    }
    let path = if input == "~" || input.starts_with("~/") {
        let home = std::env::var_os("HOME").ok_or("The host has no HOME directory")?;
        PathBuf::from(home).join(input.strip_prefix("~/").unwrap_or(""))
    } else {
        PathBuf::from(input)
    };
    if !path.is_absolute() {
        return Err("Use an absolute directory path or ~/ on the Workspace host".into());
    }
    let path = path
        .canonicalize()
        .map_err(|error| format!("Could not open directory: {error}"))?;
    if !path.is_dir() {
        return Err("The path must be a directory".into());
    }
    // Opening the directory also catches access failures before persisting it.
    std::fs::read_dir(&path).map_err(|error| format!("Could not read directory: {error}"))?;
    path.into_os_string()
        .into_string()
        .map_err(|_| "The directory path must be valid UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_existing_directories_and_rejects_invalid_paths() {
        let root = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(
            validate(root.to_str().unwrap()).unwrap(),
            root.to_str().unwrap()
        );
        for path in ["", "relative", "\0", "/uniterm-missing-directory-test"] {
            assert!(validate(path).is_err(), "{path:?}");
        }
        let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(validate(file.to_str().unwrap()).is_err());
        assert!(validate(&"/".repeat(4097)).is_err());
    }
}
