//! Stamp the build with how it relates to a release.
//!
//! `ut --version` and the About screen show `CARGO_PKG_VERSION` plus the
//! suffix computed here. A build of the commit tagged `v<version>` gets an
//! empty suffix and reports the bare version, which is exactly what the
//! GitHub release of that tag publishes. Any other build reports
//! `<version>-dev+g<commit>` (`-dirty` when the tree had changes), so a
//! binary can never pass for a release it is not, and the release verifier
//! checks for the bare form. Without git the suffix is empty.
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn main() {
    if let Some(git_dir) = git(&["rev-parse", "--git-common-dir"]) {
        for name in ["HEAD", "packed-refs", "refs/tags"] {
            let path = std::path::Path::new(&git_dir).join(name);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let suffix = match git(&["describe", "--tags", "--exact-match", "HEAD"]) {
        Some(tag) if tag == format!("v{version}") => String::new(),
        _ => match git(&["rev-parse", "--short=9", "HEAD"]) {
            Some(commit) if !commit.is_empty() => {
                let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                    .is_some_and(|status| !status.is_empty());
                format!("-dev+g{commit}{}", if dirty { "-dirty" } else { "" })
            }
            _ => String::new(),
        },
    };
    println!("cargo:rustc-env=UNITERM_VERSION_SUFFIX={suffix}");
}
