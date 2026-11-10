//! Native personal-network client. Network and durable writes run off the render thread.
mod identity;
pub mod connection;
pub mod outbox;
mod api;
pub use api::*;
pub use connection::{Connection, Endpoint, Refused};
pub use identity::{ClientKey, Directory, Enrollment, Entry, Progress, Session, prepare_create, prepare_recovery, IdentityPlan, IdentityOperationState};
pub use vibeface_protocol::workspace as wire;

pub mod local;
pub use local::{LocalTarget, LocalGrant};
