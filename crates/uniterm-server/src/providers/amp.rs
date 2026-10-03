use super::{Manifest, Rule};
/// Amp process ownership; status is cooperative or activity-based.
/// The short binary name can collide with unrelated tools, so a false
/// positive is possible; `ut agent explain` shows the process-match evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "amp",
    executables: &["amp"],
    rules: &[] as &[Rule],
};
