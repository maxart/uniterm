use super::{Manifest, Rule};
/// Kilo Code process ownership; status is cooperative or activity-based.
/// The binary name collides with the kilo text editor, so a false positive is
/// possible; `ut agent explain` shows the process-match evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "kilo",
    executables: &["kilo"],
    rules: &[] as &[Rule],
};
