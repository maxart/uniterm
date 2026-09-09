//! `uniterm-server` - the server half: the `mio` core loop, the damage-tracked
//! renderer, and the `tokio` agent runtime bridged over channels.
//!
//! Phase 0 contents:
//! - [`renderer`]: the dirty-cell diff renderer (Decision R2). Proves the
//!   zero-output-when-idle and minimal-output properties.
//! - [`runtime`]: the two-runtime skeleton (Decision R1). Proves the mio<->tokio
//!   channel boundary with a `mio::Waker`.
//!
//! Everything the hot path touches stays off the async runtime by construction.

mod chrome;
pub mod connectors;
mod context_menu;
mod control_api;
pub mod copymode;
mod dev_server;
pub mod eventlog;
pub mod file_manager;
mod git_status;
pub mod persist;
pub mod process_watch;
pub mod providers;
pub mod pty;
pub mod renderer;
pub mod runtime;
pub mod server;
pub mod terminal;
pub mod workflow;
pub mod workspace_catalog;
mod worktree;

pub use renderer::Renderer;
pub use server::{run_server, Server};
pub use terminal::Terminal;
