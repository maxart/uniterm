use super::{Manifest, Rule};
/// Factory Droid process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "droid",
    executables: &["droid"],
    rules: &[] as &[Rule],
};
