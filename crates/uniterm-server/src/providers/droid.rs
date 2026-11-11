use super::{Manifest, Rule};

/// Factory Droid resumes a session by id with `droid --resume <id>`.
pub const RESUME: &[&str] = &["droid", "--resume"];

/// Factory Droid process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "droid",
    executables: &["droid"],
    rules: &[] as &[Rule],
};
