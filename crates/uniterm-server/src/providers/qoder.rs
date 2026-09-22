use super::{Manifest, Rule};
/// Qoder process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "qoder",
    executables: &["qoder"],
    rules: &[] as &[Rule],
};
