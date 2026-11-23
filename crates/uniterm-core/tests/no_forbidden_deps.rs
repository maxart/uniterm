//! Architectural guard: `uniterm-core` must stay free of UI, async, and I/O deps.
//!
//! This is the enforced form of the invariant documented in AGENTS.md and
//! `docs/03-system-architecture.md`: the core is pure model + pure logic so the
//! multiple front-ends never diverge in behaviour. If this test fails, you added
//! a dependency that belongs in `uniterm-server` (or another crate), not here.

use std::fs;

const FORBIDDEN: &[&str] = &[
    "mio",
    "tokio",
    "crossterm",
    "ratatui",
    "termwiz",
    "reqwest",
    "rusqlite",
    "portable-pty",
    "nix",
    "notify",
    "async-std",
    "smol",
];

#[test]
fn core_has_no_forbidden_dependencies() {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let text = fs::read_to_string(manifest).expect("read core Cargo.toml");

    // Only inspect the dependency sections, not comments/prose.
    let mut in_deps = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_deps = trimmed.contains("dependencies");
            continue;
        }
        if !in_deps || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let name = trimmed.split(['=', ' ', '.']).next().unwrap_or("").trim();
        assert!(
            !FORBIDDEN.contains(&name),
            "uniterm-core must not depend on `{name}` - it belongs in uniterm-server. \
             See AGENTS.md 'The no-UI-in-core boundary'."
        );
    }
}
