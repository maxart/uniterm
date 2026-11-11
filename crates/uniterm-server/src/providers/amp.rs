use super::{Manifest, Rule};

/// Amp continues a thread by id with `amp threads continue <id>`.
pub const RESUME: &[&str] = &["amp", "threads", "continue"];

/// Amp process ownership; status is cooperative or activity-based.
/// The short binary name can collide with unrelated tools, so a false
/// positive is possible; `ut agent explain` shows the process-match evidence.
pub const MANIFEST: Manifest = Manifest {
    id: "amp",
    executables: &["amp"],
    rules: &[] as &[Rule],
};
