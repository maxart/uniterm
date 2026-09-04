use super::{Manifest, Rule};
/// Hermes process ownership; status is cooperative or activity-based.
/// The binary name collides with unrelated projects of the same name, so a
/// false positive is possible; `ut agent explain` shows the evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "hermes",
    executables: &["hermes"],
    rules: &[] as &[Rule],
};
