//! The platform surface the Node, the CLI, and this bridge share.
//!
//! Every item keeps one guarantee on each supported family: only the account running this
//! process can read a private file, connect to a local endpoint, or reach a managed process
//! tree. Unix enforces it with the owning uid, `0o600`/`0o700` modes, Unix sockets, and process
//! groups. Windows enforces it with the token user in a protected access control list, named
//! pipes, and job objects. Nothing here degrades to a permissive variant when the strict one is
//! unavailable: a location or object that cannot carry the guarantee is refused.

pub mod account;
pub mod clock;
pub mod ipc;
pub mod privatefs;
pub mod program;
pub mod secret;
pub mod stop;
pub mod tree;

#[cfg(windows)]
pub mod codex_socket;
#[cfg(windows)]
mod winjob;
#[cfg(windows)]
mod winsec;
