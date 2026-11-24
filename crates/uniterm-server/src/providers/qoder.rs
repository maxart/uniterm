use super::{Manifest, Rule};

/// Qoder resumes a session by id with `qoder --resume <id>`.
pub const RESUME: &[&str] = &["qoder", "--resume"];

/// Qoder process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "qoder",
    executables: &["qoder"],
    rules: &[] as &[Rule],
};
