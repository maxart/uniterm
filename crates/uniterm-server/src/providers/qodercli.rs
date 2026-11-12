use super::{Manifest, Rule};

/// Qoder CLI resumes a session by id with `qodercli --resume <id>`.
pub const RESUME: &[&str] = &["qodercli", "--resume"];

/// Qoder CLI process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "qodercli",
    executables: &["qodercli"],
    rules: &[] as &[Rule],
};
