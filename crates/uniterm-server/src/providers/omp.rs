use super::{Manifest, Rule};

/// OMP resumes a session by id with `omp --resume <id>`.
pub const RESUME: &[&str] = &["omp", "--resume"];

/// OMP process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "omp",
    executables: &["omp"],
    rules: &[] as &[Rule],
};
