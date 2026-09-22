use super::{Manifest, Rule};
/// GitHub Copilot process ownership; status is cooperative or activity-based.
pub const MANIFEST: Manifest = Manifest {
    id: "copilot",
    executables: &["copilot"],
    rules: &[] as &[Rule],
};
