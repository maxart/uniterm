use super::{Manifest, Rule};
/// OMP process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "omp",
    executables: &["omp"],
    rules: &[] as &[Rule],
};
