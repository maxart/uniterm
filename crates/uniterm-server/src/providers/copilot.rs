use super::{Manifest, Rule};

/// Copilot CLI resumes a session by id with `copilot --resume <id>`.
pub const RESUME: &[&str] = &["copilot", "--resume"];

/// GitHub Copilot process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "copilot",
    executables: &["copilot"],
    rules: &[] as &[Rule],
};
