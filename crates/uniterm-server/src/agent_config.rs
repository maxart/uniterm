//! Bounded, on-demand global configuration reads and explicit editor preparation.
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use uniterm_proto::AgentConfigFile;

const PREVIEW_LIMIT: u64 = 64 * 1024;

pub(crate) fn list() -> Vec<AgentConfigFile> {
    list_paths(crate::providers::global_files())
}

fn list_paths(paths: Vec<(String, std::path::PathBuf)>) -> Vec<AgentConfigFile> {
    paths
        .into_iter()
        .map(|(label, path)| {
            let mut item = AgentConfigFile {
                label,
                path: path.to_string_lossy().into_owned(),
                exists: false,
                preview: String::new(),
                truncated: false,
                error: None,
            };
            match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
            {
                Ok(file) => {
                    item.exists = true;
                    if !file.metadata().is_ok_and(|m| m.is_file()) {
                        item.error = Some("Not a regular file".into());
                        return item;
                    }
                    let mut bytes = Vec::new();
                    match file.take(PREVIEW_LIMIT + 1).read_to_end(&mut bytes) {
                        Ok(_) => {
                            item.truncated = bytes.len() > PREVIEW_LIMIT as usize;
                            bytes.truncate(PREVIEW_LIMIT as usize);
                            item.preview = String::from_utf8_lossy(&bytes).into_owned();
                        }
                        Err(error) => item.error = Some(error.to_string()),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => item.error = Some(error.to_string()),
            }
            item
        })
        .collect()
}

/// Resolve only a listed global file; opening the editor never truncates it.
pub(crate) fn prepare(path: &str) -> Result<String, String> {
    prepare_from(path, crate::providers::global_files())
}

fn prepare_from(path: &str, paths: Vec<(String, std::path::PathBuf)>) -> Result<String, String> {
    let (_, path) = paths
        .into_iter()
        .find(|(_, candidate)| candidate.to_string_lossy() == path)
        .ok_or("Choose a listed global configuration file")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                return Err("Not a regular file".into());
            }
        }
        Err(error) => return Err(error.to_string()),
    }
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_creation_preserves_content_and_previews_are_bounded() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("ut-global-files-{}", std::process::id()));
        let path = root.join("nested/AGENTS.md");
        let paths = || vec![("Instructions".into(), path.clone())];
        let _ = std::fs::remove_dir_all(&root);
        assert!(!list_paths(paths())[0].exists);
        assert!(!root.exists(), "preview must never create files");
        assert!(prepare_from("/not-listed", paths()).is_err());
        prepare_from(path.to_str().unwrap(), paths()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let content = "x".repeat(PREVIEW_LIMIT as usize + 100);
        std::fs::write(&path, &content).unwrap();
        prepare_from(path.to_str().unwrap(), paths()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        let preview = list_paths(paths()).remove(0);
        assert!(preview.exists && preview.truncated);
        assert_eq!(preview.preview.len(), PREVIEW_LIMIT as usize);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(list_paths(paths())[0].error.is_some());
        assert!(prepare_from(path.to_str().unwrap(), paths()).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
