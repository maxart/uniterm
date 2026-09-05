//! The `uniterm` binary - a thin wrapper over `uniterm_cli::run`.
fn main() {
    std::process::exit(uniterm_cli::run());
}
