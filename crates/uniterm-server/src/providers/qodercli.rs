use super::{Manifest, Rule};
/// Qoder CLI process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "qodercli",
    executables: &["qodercli"],
    rules: &[] as &[Rule],
};
