//! Personal Node coordination, authenticated replication and local SDK tools.
use anyhow::Context as _;
pub mod agent_context;
pub mod api;
pub mod client;
pub mod config;
pub mod control;
pub mod crypto;
pub mod delivery;
pub mod discovery;
pub mod fetch;
pub mod identity;
pub mod local_access;
pub mod logging;
pub mod maintenance;
pub mod manager;
pub mod model;
pub mod net;
pub mod providers;
pub mod runtime;
pub mod scheduler;
pub mod scheduling;
pub mod store;
pub mod tools;
pub mod triage;
pub mod work;

/// The CLI binary lives beside the core binary in packaged installs.  Backend processes use it
/// when they need to ask a user-facing tool or hook to continue a turn.
pub fn sibling_cli() -> anyhow::Result<std::path::PathBuf> {
    let current = std::env::current_exe().context("resolve the Core executable")?;
    let parent = current
        .parent()
        .context("Core executable has no parent directory")?;
    Ok(parent.join(if cfg!(windows) {
        "vibeface.exe"
    } else {
        "vibeface"
    }))
}
