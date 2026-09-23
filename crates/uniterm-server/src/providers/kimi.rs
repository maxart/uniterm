use super::{Manifest, Rule};
/// Kimi CLI process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "kimi",
    executables: &["kimi"],
    rules: &[] as &[Rule],
};
